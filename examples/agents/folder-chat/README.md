# folder-chat

Chat with a folder. It indexes every markdown, plain-text, source-code and PDF
file under a folder you bind, keeps that index in step as files are added,
changed or removed, and answers questions from the matching excerpts with
citations back to the file, heading or page. Embeddings, reranking and the
answer itself all go through lmgw with the models you choose in its config
form. It is an lmgw **service agent**: it serves its own UI and exposes an
MCP face, both through the same image.

## Build and install

```sh
./build.sh              # builds the folder-chat binary and the image
./build.sh --install     # also installs it into a running lmgw (stopping the app first)
./build.sh --start       # installs, then starts the app, which syncs
```

`--install` (and `--start`) needs `LMGW_OWNER_KEY` set to an owner bearer token (Usage → Keys
→ the `owner:dashboard` row's Copy button, or the `dashboard login:` line
lmgw prints to its own log at startup) and, if your gateway is not at
`http://127.0.0.1:8787`, `LMGW_URL`. The token reaches curl on stdin, never
on its command line, and a refused install (`401`, `409`) prints lmgw's
answer and fails the script.

The binary is built on the host and runs in a `fedora-minimal` image, so it
has to link against that image's glibc: `build.sh` refuses to build when the
host's Fedora release (`VERSION_ID` in `/etc/os-release`) is not the
Containerfile's base image tag. Build on that release, change the `FROM`
line to yours, or set `FOLDER_CHAT_ALLOW_RELEASE_MISMATCH=1` to build
anyway.

## First use

Bind a **folder** on the Run tab — that is the only mount field. Set
**Embedding model** and **Chat model** (both required); **Rerank model** is
optional and, when set, re-orders the best matches before they reach the
chat model. **Vision model** is optional too: set, it reads scanned and
table-like PDF pages from their images (see "Reading pages with a vision
model" below). **Chunk size** defaults to 400 estimated tokens (characters ÷ 4)
and rarely needs changing. **Serve other machines** stays off unless you mean
it — see "Who it serves" below. Press **Start**, or open the **App** tab — either
one starts the container. The first sync then walks the folder and builds the
index; watch it in the **Sync** card on the App tab, file by file. (The
service's log has only a start line and a done line, not the progress.)
Once it is done, chat.

A sync runs at start-up and on **Sync now**. **Stop sync**, shown while one
runs, stops it where it stands: the card says "Sync stopped: stopped by the
owner" (the event's kind is `stopped`; shutting the app down ends a sync the
same way, with its own reason), and the next sync picks up every file this
one had not finished — the index stays consistent, because a file is marked
current only after all of its chunks are stored.

If the GPU hold is on when the app starts, the index already on disk is
still loaded — the App tab shows it, marked as not yet checked against the
embedding model — and a question says the model is held until the hold is
released. The first question or sync after that checks the model; if the
alias now names another model, the index is unloaded and the next sync
rebuilds it.

## What it writes

The **only** thing this agent ever writes is `<folder>/.lmgw-folder-chat/` —
the index, its SQLite WAL sidecars, and, when it creates the directory, a
`.gitignore` of `*` and a `CACHEDIR.TAG`, so git, backup tools that honour
cache directory tags and many sync tools leave it alone. Nothing else under
the mount, and nothing outside it. The mount is bound **rw** because the
index lives inside the folder it indexes: rebinding the same folder later,
on this machine or another agent instance, finds the index already there and
resumes from it instead of rebuilding from nothing.

The directory is held open from start-up, and before every sync and every
file the sync checks that `.lmgw-folder-chat` is still that directory and
that the index file and its sidecars are plain, singly-linked files. If the
directory was moved or replaced, turned into a symlink, or an index file got
a second hard link, the sync stops **before writing anything** — the card
says the index directory was moved or replaced while the app ran, and what
to do (usually: restart the agent).

The index is rebuilt from nothing — and the Sync card says why — when the
embedding alias changes, when the alias is re-pointed to **another model of
the same width** (the index keeps the vector of a fixed probe sentence as the
model's fingerprint; a cosine under 0.99, `PROBE_SAME_MODEL_MIN_COSINE`, is
another model), or when the chunk size or the chunking rules change. Vectors
of two models are not comparable, so nothing is kept.

## What it skips

Every **hidden** entry (dotfiles, `.git`, and the index directory itself) and
every **symlink** (never followed) are skipped outright. Then the folder's own
**`.gitignore` and `.ignore` files** are honoured, wherever they sit and
whether or not the folder is a git repository, with git's rules (`.ignore`
wins over `.gitignore` in one directory, as in ripgrep); nothing outside the
folder is read — not `.git/info/exclude`, not a global excludes file. A
matched entry is counted as `ignored`: an excluded directory is pruned, never
walked, and listed by path in the sync report; an excluded file is counted
only. Everything else is included or skipped by file type — see Formats below;
an unrecognised extension is skipped as unsupported. A few more skips are only
knowable once a file is opened: unreadable, not UTF-8, a PDF with no
extractable text (typically a scan with no OCR layer), empty,
`pdf_timeout` (`pdftotext` did not finish within `PDF_EXTRACT_TIMEOUT`,
five minutes, and was stopped), and `too_large_for_memory` (see Limits). Every skip is counted
by reason in the sync report, and every one but an ignored file is listed
there with its path and, where there is one, the reason in words — the
numbers a size or time skip was measured against included.

A PDF with some text is indexed, but a **page** without any — a scan with no
OCR layer among typed pages — has nothing to index. Every sync names those
pages in a note on the Sync card (`pdf_pages_without_text` in the report:
the file, its page count and the pages), read from the index, so a later
sync that does not read the file still names them. An index from before
they were recorded extracts each PDF once more to record them, keeping
every vector. With a vision model set, such pages are read from their image
instead, and only the ones it could not read are named.

## Formats

Markdown (split on headings), plain text (`.txt`, `.rst`, `.org`, `.adoc`,
`.csv`, `.log`), source code (a fixed extension list, plus `Makefile`,
`Dockerfile` and `Containerfile` by name), and PDF — text extracted with
`pdftotext` (poppler-utils, installed in the image), chunked per page; with a
vision model set, scanned and table-like pages are also read from their
images, rendered by `pdftoppm` from the same package.

## Reading pages with a vision model

`pdftotext` has two gaps: a scanned page has no text layer, so nothing of it
is indexed, and a table comes out as columns of spaces, from which a small
chat model easily takes a value from the wrong column. Set **Vision model**
and the agent has that model read those pages from their images:

- a page with **no text** is read from its image alone, transcribed as
  printed;
- a page that **looks like a table** — at least 3 lines (`TABLE_MIN_ROWS`)
  that split into at least 4 cells (`TABLE_MIN_COLUMNS`) at runs of 3 or more
  spaces (`TABLE_GAP_SPACES`) — is read from its image **and** its extracted
  text: the model is told to copy every word and number from the text and to
  use the image only to see which row and column each value is in, writing
  one line per cell as `<row> | <column>: <value>`.

**Vision: read every page** (`vision_every_page`) reads every other page with
text the same way as a table. Any other page is not read. The rule and its
numbers are in every sync report (`vision_rule`).

**What is sent**: the page rendered as a PNG at 150 dpi (`VISION_DPI`) by
`pdftoppm` — the PDF's bytes piped in, as for `pdftotext`, within
`PDF_EXTRACT_TIMEOUT` — and, for a table page, its extracted text; with
`temperature` 0 and no `max_tokens`. A reading that the model's context cuts
off (`finish_reason: length`) is not stored.

**A reading never replaces the PDF's own text.** It is appended to the
file's indexed text under a line `[page N, read by <alias> from the page
image]`, cut into chunks of its own headed `page N, read by <alias>`, and its
citations say so. The chat model is told that such an excerpt is a model's
reading whose names and numbers can be misread, and to rely on the file's own
text where both are given. The MCP `read` tool returns a PDF's text with its
readings appended the same way, so a citation's lines resolve there.

**The cache.** Every reading is kept in the index, keyed by the PDF's
content (its bytes and the `pdftotext` version), the page, the alias, the
mode, the prompt version (`VISION_PROMPT_VERSION`) and the resolution
(`VISION_DPI`), so a page is read once and a re-sync reads nothing again.
The cache is not tied to the index's chunks: an index rebuilt for another
embedding model, chunk size or chunking rules reads no page again. Each PDF
records the vision settings and the readings its indexed text was built
with; changing the vision alias, **read every page**, or the rule sends
every PDF through a refresh pass on the next sync that reads only what the
new settings need and keeps every reading and vector that still applies,
and a PDF whose pass stopped part-way — a hold, a timeout, a stop — goes
through it again on the next sync. Clearing the alias drops the readings
from the index's text (they stay cached, and setting the alias again brings
them back without a model call) — except a PDF that was all scans, which is
then skipped as having no text, as before, and its readings go with it.
**The alias is the key**: pointing the same alias at another model in lmgw
does not re-read anything; changing the alias field does. Cached readings of
a file are deleted when the file leaves the folder, and at the end of every
sync that completes, those of bytes no indexed file has any more.

**Cost.** A page takes seconds — 4 to 20 with a 12B model on one GPU — and
the Sync card shows each page as it is read, so the first sync of a folder of
scans is slow. An A4 page at 150 dpi is about 1000 image tokens; a llama.cpp
model with a projector needs `ubatch_size` at least that, or llama-server
aborts on the image.

**Failures.** A page that fails the same way every time — it does not
render (or not within `PDF_EXTRACT_TIMEOUT`), the model refuses it as an
input (`400`, `413`, `422`), or its answer is empty or cut off — is an error
naming the page, the alias and why, and is cached as failed: every sync
lists it (`vision_pages_failed` in the report, and a note), and it is read
again only when the file, the vision alias or the prompt changes — or when
you press **Retry failed pages** on the Sync card (`POST /api/vision/retry`),
which forgets every failure and syncs, once you have fixed what failed (a
cut-off answer, say, after giving the model a larger context). A refusal
(`400`, `413`, `422`) is first checked with a blank probe image of the
page's own size, once per sync for each status and size: if the model
refuses that too, it refuses such images — no projector, or a context too
small for one page image — and the sync stops with the model's own error
instead of failing each page. The rest
of the file is indexed; a PDF with no text of its own whose pages all failed
stays in the index with nothing to search, so that it is still reported.
Anything that is not about the page stops the sync instead of failing page
after page: a server error or any other refusal from lmgw, an unreachable
gateway, an answer in no known shape (`vision` in the Sync card) — and a GPU
hold, or an answer from a hold fallback, whatever its status (`gpu_hold`).
Nothing of that reading is stored, the pages read before it stay cached, and
the next sync resumes. Without `pdftoppm` a page is not cached as failed: the
note says so, and the next sync tries again.

The vision model sees page images, and a table page's text, so for a private
folder it should be a **local** alias — see "Keeping it local".

## MCP tools

Two tools, `search` and `read`. Because `run.provides.mcp` is set, lmgw
proxies them onto its own aggregate `/mcp` as `folder-chat__search` and
`folder-chat__read`, reachable by any chat thread or agent that attaches the
`folder-chat` label — starting this container on demand like any other
request, the same as opening the App tab does.

`search` names the depths it ran with (`retrieval`: `k_fts`, `k_vec`,
`rrf_k`, `k_rerank`, quickdoc's names). With a rerank model, only the first
`k_rerank` (20) fused results are reranked; with a larger `k`, the rest
follow in fused order. Line numbers — in chat citations and search hits
alike — are the lines of the file **as it was indexed**, stored by the sync;
a file edited since is cited where the excerpt was then, until the next
sync.

## Who it serves

lmgw serves the app's origin, `http://folder-chat.<suffix>:<port>/`, to
whoever can reach the gateway port — it asks for no login there — so the app
decides, from two headers only lmgw can set:

- **`X-Lmgw-Face`**: `mcp` for requests through lmgw's Admin-gated
  `/agents/folder-chat/mcp`, `app` for the origin. `/mcp` answers only `mcp`
  (`403 not_mcp_face`), everything else only `app` (`403 not_app_face`). The
  tools can read whole files, so reaching the gateway port is not enough to
  call them.
- **`X-Forwarded-For`**: the address that reached lmgw. The app face serves
  only a loopback address — a browser on the machine lmgw runs on — and
  answers anything else `403 remote_client`. **Other containers on this box
  count as remote**: they reach the gateway through `host.containers.internal`
  and arrive from the host's own address. (A container lmgw starts while the
  gateway is bound to loopback reaches it through pasta's `-T` forward and
  arrives from `127.0.0.1`, so that one counts as this machine.)

Turn on **Serve other machines** (`allow_remote`) to serve the app face to
every address instead. There is no login: any machine that can reach the
gateway port and send this agent's host name can then read and chat with the
folder. The MCP tools are unaffected either way. A config change does not
restart a running app, so press **Stop** on the App tab for it to take effect.

The page may be framed only by lmgw's dashboard on this machine
(`http://127.0.0.1:<port>`, `localhost` or `[::1]` at the gateway's port —
its CSP's `frame-ancestors`), so a dashboard opened from a LAN address cannot
show it in the App tab: use **Open full page** there.

## Keeping it local

The agent sends file text only to the four aliases you picked: the
embedding model (every chunk, and each question), the rerank model (a
question's best excerpts), the chat model (the excerpts that fit the
prompt) and, when set, the vision model (the images of the PDF pages it
reads, and a table page's extracted text). If one of those aliases has a
GPU-hold fallback configured in lmgw, a hold routes the next request to that
fallback **before the agent can see it**: an embedding batch answered by a
fallback stops the sync (its vectors are never stored), so does a page
reading answered by one (the reading is never stored, but the page image has
reached the fallback), a rerank answered by one is dropped for that question
with a note saying its excerpts reached the fallback, and a chat answer is
labelled with the fallback's name. When a folder must stay on this machine,
pick aliases without a cloud fallback.

## Limits

The vector matrix is held **in memory**, whole, for as long as the container
is up — there is no paging to disk. When a sync ends, the new matrix is
loaded before the old one is released (and a question still running holds
the old one), so for that moment the vectors are resident **twice**: the
peak is double the resident size the App tab shows. A file being indexed is
held in memory whole too; one larger than a quarter of the container's memory
limit (`LARGE_FILE_MEMORY_FRACTION`, measured against the real cgroup limit,
never a fixed byte count) is skipped as `too_large_for_memory`, with the
`memory_mb` that would fit it. The manifest's `limits.memory_mb` is 2048;
raise it before pointing this agent at a folder large enough to need more.

The prompt budget — how many tokens of excerpts one answer may carry — is
derived from the chat model's own reported context length (not guessed),
minus room for the answer, the system prompt and the conversation so far, and
is shown under the chat input on every answer. The room for the answer is the
model's reported `max_output_tokens` when that still leaves room for the
system prompt and one excerpt; many servers report a model's whole context
there, so otherwise `DEFAULT_ANSWER_RESERVE_TOKENS` (2048) stands in and the
answer's notes say so. Nothing is sent as `max_tokens`: the answer itself is
not capped.

Retrieval runs with quickdoc's depths — the 50 best BM25 matches (`k_fts`)
and the 50 nearest vectors (`k_vec`), fused by reciprocal rank (`rrf_k`
60), the top 20 (`k_rerank`) reranked when a rerank model is set, at most 12
excerpts (`MAX_EXCERPTS`) offered to the budget. Every answer's notes and
`/api/status` name them. The budget keeps the best excerpts; the model then
reads them in **reading order** — grouped by file, the files in the order of
their best excerpt, each file's excerpts in their order in it — and they are
numbered that way, so `[1]` is the first excerpt shown, not the best one. A
table's header row and its values often land in separate excerpts, and in
rank order the rows can come before their header. A question asked while a sync runs ranks against
the previous sync's vectors but reads words and excerpt text live, so it can
mix the two states until the sync ends.

One question's request — the conversation so far plus the question — is read
up to the chat model's context length × `MAX_BYTES_PER_TOKEN` (48) bytes:
the budget counts 4 characters a token, and no character takes more than 12
bytes in JSON, so a conversation that fits the context never needs more. A
model that reports no context length gets the budget's stand-in,
`FALLBACK_CONTEXT_TOKENS` (8192). A larger request is refused
`413 body_too_large` with the numbers it was derived from; the current limit
is shown under the chat input and in `/api/status`.
