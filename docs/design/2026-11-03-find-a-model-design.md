# Find a model: by the name someone said, sized to the card

**Draft, 2026-11-03, revised the same day after its review** ("Review round" at the end says what
each finding changed and which part of one turned out wrong). This is part K of the newcomer
proposal, "Get a model by its name", plus the parts of part C
that need no maintained catalog: the fit marks and the recommended file in the Models page's
Hugging Face file list. A person types the name a friend said ("qwen 3.6"), lmgw asks Hugging
Face, picks one model and one file that fits this computer's graphics card, and shows one card in
plain words. "Get it" downloads the file and sets the model up without starting it, and "Say
hello" opens a chat with it. WP1 (the hub reads, the projector rule and resumable downloads),
WP2 (one plan input, the fit and the pick), WP3 (the file list's marks and its recommended
file), WP4 (search by name), WP5 (Get it, server-side) and WP6 (the finder in the dashboard) are
built (2026-11-04); nothing after them is yet. "Decisions" separates the owner's rulings from this
record's own calls. D7, D18 and D26 were ruled on 2026-11-04. "Not yet" lists what this version leaves out, and nothing there is
ruled out.

Companion records, cited by short name:
- "machine check §n": [the machine check](2026-11-03-machine-check-design.md) (the verdict,
  `MachineInCard`, the Chat's `ChatMachine` slot, the metered-connection reading D99);
- "chat plane §n": [the Chat on the Chat plane](2026-11-03-chat-plane-design.md) (the Chat's move
  into `lmgw-chat-ui`, WP3b);
- "per-model containers §n": [per-model containers](2026-08-30-per-model-containers-design.md)
  (the VRAM ledger and admission).

| § | Content |
|---|---|
| 1 | What exists today |
| 2 | Reading a file's plan before downloading it (the header-only plan) |
| 3 | What "fits" means, and which file is recommended |
| 4 | Search by name: the query, the ranking, the hub's limits |
| 5 | The card, and "Choose the file myself" |
| 6 | The flow's end: Get it, set up, Say hello |
| 7 | Send this model to a friend |
| 8 | Where it shows |
| 9 | API: ops, MCP tools, typed answers, docs |
| 10 | Tests |
| 11 | Work packages |

## Decisions

*The owner, 2026-11-03:*
1. **Go for parts C and K**, as narrowed below.
2. **No maintained model catalog for model discovery.** This feature ships nothing that needs
   keeping current: no models, no repos, no files, no list of well-known uploaders, no name
   aliases ("gemma four" → …) and no purpose tags. Ranking and matching come only from what
   Hugging Face answers live (downloads, likes, the `gguf` metadata, `baseModels`, the author, the
   tags, file sizes) and from what lmgw reads itself (the header-only plan and the card). Generic
   rules with no names in them are fine. The two catalogs that exist stay as they are: audio.cpp's
   catalog is maintained upstream, and image pipelines are too diverse to do without one. Neither is extended by this design, and it proposes no new one.
3. **Part C's starter list by purpose and its check script are out**, and so is the "start with
   one of these" fallback under the search. Part C's fit marks and recommended file in the Hugging
   Face file list are in.
4. **The full file list stays as it is.** The marks are added to it, and nothing is removed or
   hidden.

*Standing rules that bind this design:* no hidden or guessed limits (every bound is a real
quantity, and a limit that must exist is shown); no config keys as labels or in messages; confirm,
then do; a configured fallback always applies; the `/v1` routes stay true to the OpenAI API;
model licences are shown, never used as a filter or to rank lower; local LLMs are llama.cpp only;
a download never starts a model (2026-10-13); fake data in examples looks fake.

*Design calls (this record's own). D1 to D39 are this design's first pass,
and D120 and up are the review round's (D1 to D39 has no free number left). Each work package of
§11 owns a range for the calls made while building it: WP1 D40–D49 (its review round D128–D135),
WP2 D50–D59 (and D136–D140), WP3 D60–D69, WP4 D70–D79 (its review round D141–D142), WP5 D80–D89, WP6 D90–D99 (its review round D143–D146), WP7 D100–D109 (its review round D147), WP8 D110–D119, the owner's rulings of 2026-11-04 D148–D149, binary byte units D160–D164. D7, D18 and D26 were put to the
owner on 2026-11-03 and ruled on 2026-11-04 (each below says how). D175 to D179 are not design calls: they are the owner's rulings of 2026-11-04, recorded as ruled.*

*The owner's rulings of 2026-11-04 (each entry it touches says "ruled
2026-11-04"):*
1. *D69:* the band's files stay read up front; there are no "near the edge · checked when
   chosen" marks.
2. *"nemotron" on 8 GB:* `nvidia/Nemotron-Orchestrator-8B` by likes (WP4's regression set, where
   §4.4's spike had `NVIDIA-Nemotron-Nano-9B-v2`) is accepted.
3. *"qwen3 1.7b" (D114):* among the originals that match every word, the kind Find a model sets
   up comes before other kinds, then the order as it was (D15, D126). A generic rule: no names, no
   catalog.
4. *D143:* binary units everywhere, file sizes included (the owner's ruling, 2026-11-04). D143 and the code are fixed in a separate commit.
5. *D91:* Cancel on a joined get also cancels the get it joined; it stays as built.
6. *The split-GGUF fit gap* (WP8's measurement under Not yet, D3's footprint): fixed on main
   (473cf002, f86ea69f), recorded as D175.
7. *D141:* gzip stays off for the upstream LLM requests.
8. *D109:* a private original with a public upload stays as built.

**The header-only plan (§2)**
- **D1 — The plan reads the file's header over HTTP before anything is downloaded.** One
  open-ended Range request from byte 0 through the hub's redirect, read as a stream and dropped at
  the first byte of tensor data. The file's total length comes from the CDN's `Content-Range`, and
  it bounds every declared length the way `read_header` bounds them by the file's size on disk
  today (`gguf.rs:497`), so there is no size cap of its own. Why: the header plus the tensor
  descriptors is 0.07 % to 0.2 % of a large weights file (11.0 MB of 16.8 GB, measured §2.2), and
  it is everything the planner reads. For a small file the share is larger (a drafter's header is
  1.7 % of it, §2.2), and the card says what was read (D123).
- **D2 — The hub's token rules are the downloader's.** A configured token rides the hub's own hops
  only, never the CDN's (`hf/resolve.rs:51`, `get_with_commit`), and a newcomer without a token
  sends none. The spikes sent none.
- **D3 — A split GGUF is planned from its first part's header, and its weights are every part's
  bytes.** The sizes come from the tree listing. The later parts' headers are not read (16 KB
  each, measured): their only use would be an output head stored in a later part, which
  `gguf/tensors.rs:12` already documents as read from `token_embd.weight` instead. The disk side
  of the sum is finding F1, fixed separately (§1). *(Refined in WP2, D54:)* the later parts'
  headers are read after all, for the whole parameter count and the layers' width (D131), on the
  hub and on the disk alike; the plan itself still comes from part 1's header. *(Revised by the
  owner's ruling 2026-11-04, D175:)* the tensors whose place decides where llama.cpp keeps
  them are the whole set's: `token_embd.weight` stays on the host when any part holds an untied
  `output.weight`, and the output head is that tensor in whichever part holds it
  (`ModelSummary::of_set`). The plan, the fit, the card, `model_inspect` and admission read the
  later parts for it alike (`plan_files::set_summary`, `vram::plan::split::summarize`), so a
  split set and the single file of the same quant cost the card the same, the parts' own
  headers aside. When a later part isn't read, the set is part 1's alone, as before.
- **D4 — Remote headers are cached in memory by the file's content hash** for the life of the
  process, so the card and the file list never read one file twice. *(Revised after review.)* The
  key is the tree listing's `lfs.oid` (the sha256), which the listing gives before any read, and
  which equals the resolver's `x-linked-etag`. A file whose hash changed is read again. `HfFile`
  parses the `lfs` object since WP1 (`HfFile::oid`), and the cache is `hf::remote_header` (D43).

**What "fits" means (§3)**
- **D5 — "Fits" has two measures, and both are admission's arithmetic** *(revised after review)*.
  *The whole card*: the declared GPU memory budget, else the devices' total, plus the headroom
  setting (`plan_fit.rs:26`, `Card`), with nothing else on it, because admission unloads lmgw's
  other models when this one is asked for. *Beside what runs now* (D120): the same card less the
  ledger's outside use and less the charges of lmgw's own rows on the card now. Which of the two
  the card leads with is D120.
- **D6 — The context shown is the largest that fits, up to the model's trained maximum,** found by
  the planner's own search (`plan_fit.rs:131`, `over_card`) with the file's projector and drafter
  included. It is shown as "How long a conversation it can remember · context, 131 072 tokens",
  with "the model can take 262 144" after it when the card fits less. There is no rounding.
- **D7 — The recommended file. Ruled by the owner 2026-11-04: the review's proposal.** The
  recommended file is the largest file that fits with the model's whole trained context; when no
  file does, the largest that still fits a conversation of at least 32 768 tokens (or the trained
  context, if that is smaller); never a file below 4 bits per weight. Both numbers are this
  design's choices, not limits: they decide which file gets the mark, every file stays
  choosable, and the card's "Why this file" line names them. With four refinements:
  - the parameter count comes from the header's tensor descriptors (each tensor's element count,
    summed), not from the hub's `safetensors.total`, so a GGUF-only upload has one too and the
    count is the file's own (the hub's listing count disagreed with the file for
    `google/embeddinggemma-2`, §4.4);
  - bits per weight are measured over the repeating layers only (the `blk.*` tensors' bytes times
    8 over their element count), so a file whose token embeddings are kept wide doesn't read as
    wider than its layers (the spike let `phi-4-Q3_K` through at 4.02 bits over the whole file,
    §4.4);
  - the pick is capped at the Q8 class, at most 8.5 bits per weight over the repeating layers
    (Q8_0's own width), so a small model that fits in BF16 or F32 gets its Q8_0. The wider files
    stay in the list with their marks;
  - when the pick fits only a shorter conversation and a smaller file of at least 4 bits fits the
    whole trained context, the fold names that file: "Q4_K_S remembers all 262 144 tokens, at 4.6
    bits per weight."

  *Built in WP2* (`modelinfo/pick.rs`, `FilePick`): the two rules, the floor and the cap as
  `FLOOR_BITS`, `CAP_BITS` and `SHORTER_TOKENS`, equal sizes by name, and only a file whose header
  was read takes part (D56). The fold's file is `full_context_for` (D53). Which of the `blk.*`
  tensors the width is measured over, so a real Q8_0 reads its 8.5 bits, is D140. A header that
  states no context gets a "Why this file" line without a token count.
- **D8 — When a header can't be read, the mark falls back to size alone,** "fits by size · not
  checked" or "doesn't fit", and such a file is never recommended. The reason is in the fold.
  *(Refined in WP2's fix round:)* the fold carries the real reason (the read's error, or the
  refusal of a target the hub plan doesn't read headers for, which is checked before any read).
  Bytes the listing doesn't give (a file without a size, a split file with a part missing from
  the listing) are unknown: the mark is `unknown`, never a fit, with the missing part's sentence.
- **D9 — With nothing to measure against, the card asks for the card's memory** *(revised after
  review)*. That is the case when `Card::of` has no capacity (`plan_fit.rs:40`): no GPU telemetry
  and no declared budget. The card says lmgw can't measure the graphics card yet, shows the
  machine check's line (`MachineInCard`, `widgets/machine_check/surfaces.rs:126`), and asks inline
  "How much memory does your graphics card have? · VRAM, GB". The answer is saved, after a
  confirm, as the existing GPU memory budget setting (the same field Settings shows), and the
  marks follow from it. Getting a model stays possible without an answer.
- **D10 — The GPU hold doesn't change the marks.** It adds one line *(revised after review: it
  says where to resume)*: "lmgw is paused on the graphics card right now. The model answers once
  you resume it: the HOLD button at the foot of the sidebar, or the tray menu's GPU hold." lmgw
  never releases the hold for this.
- **D11 — A machine check that isn't `ready` doesn't block "Get it".** A download needs no
  graphics card. The card shows the machine check's line, and the first message then meets the
  `gpu_setup` card the Chat already has (machine check §2.5).

**Search by name (§4)**
- **D12 — The search is Hugging Face's own model search, in two calls**: every repo, and GGUF
  repos only, each sorted by downloads, at the hub's page size. "Show more" follows the hub's
  `Link` cursor, so no result is out of reach. The search runs when the person presses Find or
  Enter, never as they type.
- **D13 — A name matches by its words, counted** *(revised after review)*. The query and each repo
  name are split into words at spaces, `-`, `_`, `/`, and where a letter is followed by a digit
  ("qwen3.6" → qwen, 3.6; "27b" stays one word). Each query word is exact (it is one of the name's
  words), loose (it only starts one) or missing. Repos rank by the number of exact words, then of
  loose ones. A repo with every query word exact or loose can be a card. One with words missing is
  only a "Did you mean" line, ranked by how many words it matched.
- **D14 — The card shows an original model, not an upload of it.** A matching repo is an original
  unless one of its base models also matches the query and is either by another author or a
  quantization of it. So `Qwen/Qwen3.6-27B` is an original, its FP8 twin by the same author is
  not, `google/gemma-4-31B-it` (a finetune of Google's own base) is, and an abliterated GGUF of
  Qwen's model by someone else is not. *(Revised after review.)* The maker's own GGUF repo (same
  author, the original's name plus `-GGUF`, no weights of its own) is folded into its original as
  an upload, even when the hub lists no base model for it (`Qwen/Qwen3-Embedding-0.6B-GGUF`). An
  original needs a GGUF to be shown: its own, or an upload's.
- **D15 — Originals rank by match, then by having weights of their own, then by likes**
  *(rewritten after review B1)*. "Weights of its own" means the hub's `safetensors` expansion
  names a parameter total, or the repo is tagged `pytorch` (`BAAI/bge-m3` has only a
  `pytorch_model.bin`). A GGUF-only repo comes after every original with weights of its own,
  whatever its likes. Downloads break a tie in likes. The `conversational` tag no longer ranks
  across authors: it is missing on many real originals (`Mistral-Small-3.1-24B-Instruct-2503`,
  `DeepSeek-V4-Flash-0731`, `gemma-4-E4B-it`), and an upload of someone else's model often has it.
  Ranked by the tag first, "mistral small" put a 0.5B draft model first, "deepseek v4 flash" a
  Qwen finetune, and "gemma 4" an uncensored upload above Google's own E4B (§4.4). *Within the top
  original's author*, a chat model comes before a base one: for a chat card, one the hub tags
  `conversational` or a finetune of a matching original by the same author; for an embedding or a
  rerank card, one whose pipeline is that kind's (D126).
  *Ruled 2026-11-04 (ruling 3, on "qwen3 1.7b", D114):* the kind leads. Among the originals, all
  of which match every query word (D13), one of a kind Find a model sets up (chat, embedding or
  rerank by D126's pipeline rule: the kinds `model_get` creates a row for) comes before one of
  any other kind (D30's); then match, weights of its own, likes and downloads as above. The
  partial matches' order (D13) is unchanged. So "qwen3 1.7b" answers `Qwen/Qwen3-1.7B` (614 likes, as recorded on 2026-11-04),
  where `Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice` (text-to-speech, 2202 likes, matching every word as
  exactly) answered another kind, and the speech model is listed under "Other models with this
  name", after every original of a kind Find a model sets up. The answer is another kind only
  when no original of such a kind matched (D148).
- **D16 — The file comes from a plain upload of the original.** A plain upload's base model is the
  original with relation `quantized`, and its name is the original's name, optionally with a
  prefix and a `-GGUF` ending (`unsloth/Qwen3.6-27B-GGUF`, `bartowski/Qwen_Qwen3.6-27B-GGUF`). The
  maker's own GGUF comes first, then uploads by likes, then by downloads. Renamed uploads (an MTP
  variant, an abliterated one) stay listed under "Other uploads of this model".
- **D17 — The card shows the best original of the top author that has a file that fits**
  *(revised after review B1)*. The walk tries the top original's author's originals in their rank
  (D15) and never crosses to another author: when none of them has a file that fits, the answer
  is `NothingFits`, with the top original's card saying so. Other authors' originals are listed
  under "Other models with this name", each with its fit read when it is opened. An original is
  passed over with no call when its parameter count at 4 bits per weight plus the headroom is
  already larger than the card (the count is the listing's `safetensors.total`, which rides the
  search call), and after two calls (its uploads, and the first upload's tree) when its smallest
  file of at least 4 bits plus the headroom is. *(Revised in WP4's review round.)* Two calls is
  an original whose first upload answers; every gated, weightless, out-of-band or unanswered
  upload before the one used costs its tree's call too, and a long tree one call a page. A walk
  set of hundreds of the same author's originals with no parameter count could cost hundreds of
  calls: lmgw adds no cap, the hub's `RateLimit` is the bound, and an answer it stops partway
  keeps what was found (D76). `NothingFits`' card is the original closest to fitting (its
  smallest file in the band, or its size at 4 bits when it was passed over), the first of
  equals, not the first in the walk's order: "deepseek v4 flash" now names
  `deepseek-ai/DeepSeek-V4-Flash`, smaller than the `-0731` §4.4's table names.
- **D18 — Number words. Ruled by the owner 2026-11-04: the first draft.** No table of number
  words, no aliases (ruling 2) and no retry, so "gemma four" finds no runnable model and the
  answer says names are usually written as on the model's page, with numbers as digits. The
  owner judged even a fixed word-to-digit rule over-engineered ("Approved, even the word to digit
  transformation is over-engineered").
- **D19 — "by …" is the author's own display name on Hugging Face** (an organisation's or a user's
  `fullname`), so `Qwen/Qwen3.6-27B` says "by Qwen". The proposal's "by Alibaba" would need a list
  of who owns which organisation (ruling 2). It costs one API call per card
  (`/api/organizations/<author>/overview`, else `/api/users/<author>/overview`), counted in §4.4.
- **D20 — A gated upload is passed over for the card when a plain upload of the same model that
  isn't gated has a file that fits.** This is about access, not the licence: the two are the same
  model, and one can be fetched now. When every upload is gated, the card says the maker asks
  people to accept its terms on Hugging Face first, links there and to the Hugging Face token
  setting. Licences are shown and never filter (standing rule).
- **D21 — Unreachable hub, rate limit, no match, no runnable file: one plain sentence each, no
  retry loop** (§4.5). *(Revised after review M5.)* `NoRunnableFile` is new: an exact match exists
  on the hub, but no GGUF of it does.

**The card and the flow (§5, §6)**
- **D22 — The card's words come from facts, not descriptions.** The name is the original's repo
  name with hyphens as spaces ("Qwen3.6 27B"). Next to it is "by …" (D19), then what it can do,
  derived as `for_local_row` derives it today (`capabilities/mod.rs:245`): "reads pictures" when
  the upload ships a projector, "thinks before it answers" when the chat template has a thinking
  switch, and the parameter count from the header ("27 billion parameters", D7). Then come the
  fit, the download size with its time, and the licence, linked to the original's page on Hugging
  Face, where the hub shows the licence text *(revised after review)*. There is no "good at
  writing and ideas": that would need someone to write it per model.
- **D23 — The download time is measured** *(revised after review)*. It comes from this machine's
  recent finished Hugging Face downloads, weighted by bytes: their bytes summed over their run
  times summed (from each job's `started_at` and `finished_at`, `jobs/mod.rs:180`), so a 50 MB
  projector doesn't weigh like a 17 GB file. Before there is one, it comes from the header reads
  the card just made, which come from the same CDN. The time includes the engine image when one
  must be fetched (machine check §5 gives its size). It reads "about 4 minutes on your
  connection", rounded up. During the download the line counts down from the live progress.
  *Built in WP2* (`hf/rate.rs`, `DownloadEstimate`; D58 says which jobs count and how a header
  read is timed). The engine's bytes are the registry's layers less the ones Podman's image store
  has (`MachineChecks::layers_here`, §1.6's read of the store's `layers.json`); when the store
  can't be read, every layer counts and the answer's note says the time may be shorter. The
  countdown during the download is WP6's: *built in WP6* from the get's own bytes on the jobs
  feed (D92).
- **D24 — "Get it" is the confirmation.** The card names the size, the time, and the files
  (the weights and, when there are any, the projector of D121 and the drafter). One press queues
  them, and nothing downloads before it.
- **D25 — The server sets the model up when its files are done, not the page.** The page may close
  or the window may go. A job of its own kind waits for the downloads, plans against the files on
  disk, and creates the row (§6.2). Today the wizard does this from the page (`wizard.rs`,
  `finish`), and it is lost when the page goes. The job also outlives a restart (D122).
- **D26 — The row K creates takes the largest context that fits. Ruled by the owner
  2026-11-04.** When the card fits less than the trained context, K's row takes the largest
  context that fits. Owner finding 3 (2026-10-13, `modelinfo.rs:1539`) keeps the trained context
  as the plan and warns instead. This one path departs from it, because a newcomer's first
  message would otherwise be refused at load. The departure is accepted, and recorded. The row keeps a note of the fit: the context it got, the
  measure it was fitted to (the whole card or beside what ran, D120) and that measure's bytes, and
  when. The model editor shows the value as changed from the plan with that note, and offers "Fit
  again", which re-runs the fit against the card as it is then and saves the new context after a
  confirm. For an aux row the fit re-plans at the fitted context: an embedder's or a reranker's
  plan ties the ubatch and the batch to the context (`aux_plan.rs:394–404`), so all three move
  together. *WP2 provides the fit and the note's data, WP5 writes them* (D139): the fitted
  parameters (`fit::fitted_params`, the tied ubatch and batch moving with the context) and the
  note (`FitNote`: the context, the trained one, the measure, its bytes and when). The re-fit of a
  loaded row isn't fitted beside itself: the beside measure leaves out every resident whose row
  serves the planned file (D55). A fitted context below a vision projector's ubatch floor needs
  nothing more: llama.cpp lowers the ubatch to the context for a causal model and admission's
  compute buffer does the same (`vram/compute.rs`, `reserved_ubatch`), and an image longer than
  the context is refused by llama-server's own context check before it is decoded, so the abort
  the floor guards against can't happen there; such a row only takes no image that long.
- **D27 — Setting up never starts the model** (the 2026-10-13 ruling). "Say hello" creates a chat
  thread on the model (`POST /chat/api/threads`, `web/mod.rs:248`) under the model's public name
  and opens it, and the first message loads the model like any request. *(Revised after review:
  the route's place, and the public name, `Snapshot::local_public_name`, `config/snapshot.rs:450`,
  which carries `router.public_prefix` when one is set.)*
- **D28 — The model's id is the original's repo name in lower case** (`Qwen/Qwen3.6-27B` →
  `qwen3.6-27b`). An id is taken when its public name is any name already served on `/v1` *(revised
  after review)*: a plain alias, a candidate alias, or the public name of a row in any of the four
  classes, case-insensitively, as `candidate_alias_name_taken` (`ops/common.rs:391`) checks today.
  Ids alone may repeat across classes (`ops/common.rs:229` asks for the target then), but public
  names may not. A taken one gets `-2`, `-3` and so on. An embedder's row is served under the aux
  prefix as `embed/<id>`, as every aux row is.
- **D29 — An embedder ends in "Ready for Knowledge", not "Say hello".** The class is the header's
  (`class_for`, `modelinfo.rs:184`), read before the download, so the file lands in the aux models
  directory. A reranker says "Ready for Knowledge" as well, and a decision model says "Ready for
  decisions" with a link to its lab.
- **D30 — A model K doesn't set up is pointed at the page that does.** A hit whose pipeline is an
  image or speech one, or whose header names a kind K doesn't plan, gets one sentence and a link:
  Models → Add from recipe for pictures, the audio catalog for speech. That links to the
  catalogs; it doesn't extend them.

**Sharing, places and API (§7 to §9)**
- **D31 — "Send this model to a friend" shares the file's public Hugging Face address**,
  `https://huggingface.co/<repo>/blob/main/<file>`, in a short text that says to paste it into
  lmgw's Find box. lmgw's Find box takes any Hugging Face address (a repo, a file, a download
  link) or `owner/repo`, and plans that file for the friend's own card. The link carries the repo
  and the file, and nothing else. A private repo gets no link (D124).
- **D32 — No address of the gateway goes in a link.** A link to `http://127.0.0.1:8001/…` would
  open nothing on the friend's computer unless their lmgw happens to use the same address, and it
  would tell them the sender's address. An `lmgw:` link that opens lmgw itself is not yet (Not
  yet).
- **D33 — The finder is one component with its own route.** It is `ModelFinder` at
  `/models/find?q=…` (or `?hf=…`), with a "Find a model" button on the Models page beside "Add
  from Hugging Face", which stays. The Overview's empty state gets an entry until part B's
  checklist exists. Part J's Chat empty state hosts the same component later, through a slot like
  the one the machine check gives the Chat (§8.3).
- **D34 — "Choose the file myself" opens today's file list in a pick-only mode** *(revised after
  review M7)*: the `HfWizard` on the card's upload, with K's pick selected and the marks of §3 on
  every file, whose button reads "Use this file" and hands the file back to the card. The card's
  "Get it" then gets that file with K's fitting context. It never leads to the list's own
  "Download & create", which creates the row at the trained context (D35) that admission would
  refuse on this card.
- **D35 — The file list's own "Download & create" keeps creating what the planner plans**: the
  trained context, with finding 3's warning when it doesn't fit. Only the marks and the preselected
  file are new there. K's row takes the fitting context (D26) because the card promised it.
- **D36 — Four API pieces.** `model_find` is a read, `model_get` a write, `hf_repo` gains the
  marks, and `local_model_plan` gains `repo` and `file` for a plan from the hub. Each is shared
  with an MCP tool, `lmgw__model_find` and `lmgw__model_get` being new (§9). *(Revised after
  review M9.)* `hf_repo`'s `fit` defaults to false for every caller, because each header it reads
  costs 1 to 16 MB (§2.2): the dashboard's file list asks for it, and an MCP caller asks when it
  wants the marks. The tool's description says what a read costs.
- **D37 — The answers are typed** in `lmgw-api-types` (`model_find.rs`, and `downloads.rs` for the
  marks), with JSON schemas, as every answer is since v0.4.4.
- **D38 — The tests run against recorded Hugging Face answers,** checked in as fixtures. A fixture
  is a recording of what the hub said, not a list lmgw ships or consults. The expected cards of
  §4.4 are test expectations over those recordings.
- **D39 — Everyday words first, the term as the hint, one label at every depth**: "How long a
  conversation it can remember · context", "Memory on your graphics card · VRAM", "fits your
  graphics card", "doesn't fit". The expert reads the same labels with the numbers.

**The review round's calls (2026-11-03)**
- **D120 — The card leads with the file that fits beside what runs now** (review M1). The whole
  card is the wrong measure for two of the people this is for: a gamer whose game holds 6 GB while
  they search, and a Kai user whose voice and embedder rows stay resident for every conversation.
  So the fit is taken twice (§3.1): against the whole card (D5) and beside what runs now (the
  card less the ledger's outside use, `outside_of`, `vram/beside.rs:156`, and less the charges of
  lmgw's own rows on the card now, `ResidentView.estimated_bytes` plus their kept-free peak, as
  `crowded` adds them up, `vram/beside.rs:105`). When the two picks differ, the card shows the
  beside pick, and one line under it the whole-card one: "With the other programs closed: Q5_K_M,
  262 144 tokens · Use this one instead". When nothing fits beside them, the card shows the
  whole-card pick with "fits once <the programs, or lmgw's voice models> are closed". Why beside
  first: what runs now is the only fact lmgw has about how the card is shared, and a pick that
  fits only after closing the game or unloading Kai's voice surprises at the first message. The
  set-up job fits to the measure the card showed (D26's note). *Built in WP2* (`modelinfo/fit.rs`,
  `VramScheduler::on_card_now`, `CardNow::beside_itself`; D55 says what the beside measure
  charges).
- **D121 — One projector rule, shared by every place that picks one** (review H1; *refined in
  WP1, built*). Among the projector files in the weights' directory, the pick is the smallest one
  whose quant label is F16 or BF16, else the smallest one, equal sizes by name. The label comes
  from the name (`quant_label`, now `hf.rs`, its width the label's last word, D41) and the size
  from the tree listing or the disk, so `hf_add`, the hub plan, the disk plan and `model_get`
  decide alike from the same two facts (`hf::pick_projector`, `hf/projector.rs`). Only that file
  is fetched and charged. For `unsloth/Qwen3.6-27B-GGUF` that is `mmproj-F16.gguf`, 0.93 GB, where
  `hf_add` fetched all three before WP1 (3.70 GB, 2.77 GB more than one). Every other projector
  stays in the file list, choosable. WP1 also puts the aux plan's projector pick
  (`modelinfo/aux_plan.rs`) and the capabilities note that names a sibling projector
  (`capabilities/exposed.rs`, `sibling_mmproj`) on the rule. *(After WP1's review:)* the note
  takes its candidates as the disk plans do, a `.gguf` being a projector when its header says so
  (through the summary cache), and its size the file a symlink points at.
- **D122 — A confirmed download, and a Get it, outlive a restart** (review H2; *refined in WP1,
  whose download half is built*). A desktop app is often quit between sessions. Boot closes every queued and
  running job row, and before WP1 it queued the cut downloads again from byte 0 (D45). The intent
  is the record: `hf_models` keeps the file's hash (`hf_models.oid`, migration 0097, the hub hop's
  `x-linked-etag`, which is the listing's `lfs.oid`) when a transfer starts at byte 0 (D42), and
  a Get it is a row of its own (§6.2). At boot, after the orphaned jobs are closed, each download
  that was running or queued is queued again (`jobs::hf_download::resume_at_boot`) and resumes
  from its `.part` with a Range request from the byte it reached, when the hub still names the
  same hash and the CDN answers `206` from there. Else it starts again from byte 0 and says why:
  the file changed, the hub named no hash, or the CDN sent the whole file (D46). A Get it
  re-attaches to its downloads and goes on (WP5). On a metered connection (machine check D99,
  read as the engine pre-fetch reads it, `machine::connection_metered`) nothing resumes by
  itself: the row stays `queued` and says "Paused: metered connection" with "Resume", because the
  person confirmed the download on another connection (D47), until NetworkManager reports a
  connection that isn't metered (D133). Cancel still deletes the `.part`, and forgets its hash,
  on a paused row too. *(After WP1's review:)* what is renamed into place is checked against the
  hub's length and sha256 (D130), a `.part` that is the whole file already is finished without a
  download (D132), the hash is recorded only once the `.part` is emptied (D128), and a transfer
  that the CDN stops sending to fails after 30 s (D134).
- **D123 — The card shows its size marks first, then the header's, and says what it read**
  (review M3). `model_find` reads no header. Its card comes from the listings with size-only marks
  (D8's `BySizeOnly`) in about a second, and the page then asks `hf_repo` with `fit=true` on the
  card's upload, which reads the one candidate's header (and its drafter's), estimates the other
  files from it (§3.3), and replaces the marks and, if the header says so, the pick. Headers are
  read once per original: every file and every plain upload of one original share its tokenizer
  and shape, so the walk of §4.3 estimates the next file down from the header it has. The answer
  and the fold say how much was read ("lmgw read 11.0 MB of the file's header to check this"). On
  a metered connection the page doesn't read headers by itself: the marks stay by size, with
  "Check exactly (reads 1 to 16 MB)". *(WP2:)* `source.header_bytes_read` counts the bytes this
  answer fetched; a header the process had read before (cached by its hash, D4) adds nothing.
- **D124 — A private repo shows, but has no share link; an address that isn't Hugging Face's is
  refused locally** (review M4). With a token set, the hub's search returns the account's private
  repos too. The listings ask for `private`, and a private model's card has no "Send to a friend":
  a friend couldn't open the link, and it would name a private repo. A pasted address on another
  host, or any `scheme://` text that isn't `huggingface.co` or `hf.co`, is answered locally ("lmgw
  finds models on Hugging Face. This address is on <host>.") and never sent to the hub as a
  search.
- **D125 — Free disk space is checked before Get it, as a warning** (review, low). The card reads
  the models directory's free bytes (`free_bytes`, `backends/paths.rs:162`, statvfs) against the
  files' and the engine image's sizes. Short of room, the card says "Needs 16.8 GB on the disk,
  9.1 GB is free" and Get it asks once more before it queues, as a build's footprint warns today
  (a warning, never a block). A download that runs out of space fails with that sentence.
- **D126 — The walk also keeps the top original's kind and match** (this round's own, from the
  spike of §4.4; the review asked for the author only). The kind comes from the hub's pipeline
  names, a fixed rule over the hub's own taxonomy, not a model list: `feature-extraction` and
  `sentence-similarity` make an embedder, `text-classification` a reranker, and
  `text-generation`, `image-text-to-text`, `any-to-any` or none a chat model; anything else is
  D30's. The match is the top's count of exact and loose words (D13). Without the kind, "glm" on a
  24 GB card walked from GLM-5.2 to GLM-OCR, an OCR model. Without the match, "gemma 4" on an 8 GB
  card walked to `gemma-3-4b-it`, which matches "4" only loosely through "4b".
  *Ruled 2026-11-04 (with D15):* the top original is the first of a kind Find a model sets up
  whenever one matched, so the walk keeps that kind and that original's match; it starts from
  another kind (D30's answer, no walk) only when every original is of another kind.

**WP1's calls (2026-11-04, the build's own)**
- **D40 — The caller's headers ride the first request off the hub, and nothing else.**
  `get_with_commit` takes a header map for the CDN's hop and returns `HubHop` (the commit,
  `x-linked-etag` unquoted, `x-linked-size`), each from the last success or redirect on the hub
  that names it, as the commit always was. A hub that answers a file itself (a file kept in git,
  a mirror, a mock) gets no Range, so it answers the whole file, and both callers read that `200`
  as the whole file. Why: the hub's own hops have no use for a Range, and sending one there would
  make a hub-served answer partial in a way neither caller checks for.
- **D41 — A projector's width is the last word of its quant label.** `quant_label` takes the
  earliest of its markers (`-Q`, `-F16`, …), so `mmproj-Qwen…-F16.gguf` labels as the whole
  `Qwen…-F16`. The rule reads the label's last `-` word, case aside, so every 16-bit projector
  name seen on the hub counts as one. A candidate whose size is unknown is never the smaller one.
  The drafters are left as they were: `hf_add` still queues every drafter beside the weights and
  the disk plan takes the first one its scan lists, because a repo with two drafters in one
  directory wasn't seen.
- **D42 — The resume hash is a column of its own, written from the hub's hop when a transfer
  starts at byte 0** *(refined after WP1's review: only once the `.part` is emptied, D128)*.
  `hf_models.oid` (migration 0097), not the listing's `lfs.oid` at queue
  time: the transfer is where the bytes come from, and the hop names the hash of exactly those
  bytes. `hf_models.etag` stays the final response's ETag, which is what `remote_etag`'s
  redirect-following HEAD compares for the update check: read in code, that is the CDN's own
  ETag (the xet hash), not `x-linked-etag`, so §2.2's "the sha256 … is what lmgw's update check
  already compares" holds for neither and the update check is unchanged by WP1.
- **D43 — The header cache keeps the summary, under the hash the hub named for what was read.**
  `hf::remote_header(state, repo, &HfFile)` looks up the listing's `lfs.oid`, reads on a miss
  (one read per hash at a time), and stores the result under the hop's `x-linked-etag`, else the
  listing's. A listing older than the file then misses and reads again. A file with no `lfs`
  object in its listing (kept in git, never a GGUF on the hub) isn't cached. The cache lives on
  the state (`AppState::hf_headers`) with a read counter, as the disk summary cache does. It
  reads at `main` only, as the finder does. *(After WP1's review:)* the result is stored under
  the hash lowercased, as the lookup asks for it, and a hash's guard is dropped from the in-flight
  map once its read is over and cached, so the map holds only the reads under way. A read off the
  network is bounded and says so (D129).
- **D44 — A header read stops after 30 s without a byte, and says so.** The hub hop and every
  chunk get the tree listing's page bound (`hf/listing.rs`). A slow line that keeps sending is
  never cut. *(Refined after WP1's review:)* the downloads take the same bound (D134).
- **D45 — Boot already queued the cut downloads again; WP1 makes that a resume.** §1 says boot
  fails every queued and running job, which is true of the job rows (`fail_orphaned_jobs`,
  unchanged), but `spawn_background_tasks` (`server.rs:883`) then queued every `hf_models` row
  left `queued` or `downloading` again, from byte 0. That pass is now
  `jobs::hf_download::resume_at_boot`, called from the same place: it asks the metered question
  once, and each transfer continues its `.part`. The Downloads page's "interrupted" row with
  "Resume" was already there for a row no transfer runs.
- **D46 — Why a `.part` wasn't continued is said in the transfer, not on the row.** The job's
  progress detail and its result carry `resumed_from` (the byte it went on from) and `restarted`
  (the sentence), and the log line repeats it. The row's `error` stays for failures. The cases:
  the hub names another hash; the hub names none; the CDN answers `200`; the CDN answers `416`
  (nothing past the `.part`) for a `.part` that isn't the whole file. A `206` of the wrong file or
  offset is dropped and the file asked for again without a Range ("sent another part of it than
  the rest"); a `200` is written as it comes. *(After WP1's review:)* a `416` over a `.part` that
  is the whole file finishes it (D132).
- **D47 — The metered pause is a queued row with a sentence, not a new status.** Boot leaves
  each row `queued` with `PAUSED_METERED_NOTE` as its `error`, so no table rebuild for a new
  `status` value, and the Downloads page shows "paused: metered connection" (the sentence as its
  title) with the "Resume" it already offers a queued row nothing transfers. "Resume" is
  `hf_set action=redownload`, which goes on from the `.part`. Only the boot pauses: a Resume, a
  Retry or an `hf_add` pressed on a metered connection is the person's own say and goes ahead.
  What NetworkManager can't tell counts as not metered, as the pre-fetch counts it, and the boot's
  log says why (after WP1's review). *(Refined after WP1's review:)* a paused row also goes on by
  itself once the connection isn't metered (D133), and Cancel works on it.
- **D48 — A failed transfer keeps its `.part`; a short one fails.** Retry goes on from where a
  failed transfer stopped (it always kept the `.part`; now the next transfer uses it). A body
  that ends short of the length it was sent with fails with "ended at X of Y; Retry goes on from
  there" instead of being renamed into place. *(Refined after WP1's review:)* the length is the
  response's or else the hub's `x-linked-size` (D130), a body longer than it is deleted, and a
  `.part` whose bytes fail their sha256 is deleted with its hash (D130); besides those, only a
  cancel deletes the `.part` and its hash. What is kept shows on the Downloads page (D135).
- **D49 — The header's new facts are the file's own.** `GgufMeta` gains each tensor's element
  count and the data start, `ModelSummary` the parameter count and the repeating layers' bits per
  weight (`blk.*` only, so a projector's `v.blk.*` gives none). *(Refined after WP1's review,
  D131:)* a split file's part has no parameter count of the model, only its own share
  (`part_parameter_count`), and `gguf::params::whole_parameter_count` adds the parts' shares up.
  Neither fact is on any wire yet; WP2's pick and WP3's marks are their first readers. The
  functions are `gguf/params.rs`.

**The review round of WP1 (2026-11-04)**
- **D128 — The resume hash is written once the `.part` is emptied, never before.** A transfer
  from byte 0 creates (truncates) the `.part` first and records the hop's hash after. Before the
  review it recorded the hash first, so a quit, a crash or a failing `create_dir_all` or
  `File::create` in between left the old file's bytes under the new file's hash, and the next
  transfer appended the new file's tail to the old head with the right length. In the new order a
  stop in between leaves an empty `.part`, which a transfer starts again. A `.part` left that way
  by the build before is caught by its sha256 (D130).
- **D129 — A header read off the network stops at the memory available, and says so.** Strings
  are read in 64 KiB steps through `try_reserve`, so a string holds only the bytes that arrived
  and memory the system can't give is an error, never an abort (before, a 19 GB string declared
  under a 20 GB total asked for all of it before reading a byte). A read off the network also
  stops before any declared length would carry the header past `MemAvailable` (read from
  `/proc/meminfo` when the read starts; `gguf::read_header_within`), with "the header of X
  declares 17.7 GiB of metadata, more than the 2.0 GiB of memory this computer has available, and
  llama.cpp holds all of it in memory to load the file, so lmgw stopped reading after …", the last figure being the bytes it received.
  Why that quantity: llama.cpp's loader keeps every metadata value in memory, so a header past it
  couldn't be loaded here anyway, and it is the computer's own figure, read each time. Without
  `/proc/meminfo` the total alone bounds the read, as on disk, and the log says so. A real header
  is 11 to 16 MB (§2.2).
- **D130 — A finished download is the file the hub named, by length and by sha256.** The length
  is the response's (`Content-Length`, or a `206`'s `Content-Range` total), cross-checked with the
  hub's `x-linked-size`, and the hub's when the response gives none (a chunked body); a mismatch
  between the two fails before anything is written. A fresh transfer of an LFS file (every GGUF)
  hashes its bytes as they arrive; a resumed one reads its `.part` once, front to back, before
  asking for the rest, with "checking 40%" on the Downloads page's chip, and goes on hashing.
  The sha256 must be the hub's `x-linked-etag`. A mismatch, or a body longer than the length,
  deletes the `.part`, forgets its hash and fails with "the downloaded X doesn't match the sha256
  Hugging Face names for it (…), so lmgw deleted it; Retry downloads it again from the beginning".
  A file the hub keeps in git has no `lfs` hash (`x-linked-etag` is absent) and is checked by its
  length alone. The check costs no extra read on a fresh download and one read of the `.part` on
  a resume.
- **D131 — A split file's part counts its own parameters, not the model's.**
  `ModelSummary::parameter_count` is `None` for a file whose `split.count` is above 1, and
  `part_parameter_count` holds the part's share, so nothing reads a part's count as the model's.
  `gguf::params::whole_parameter_count` adds every part's share up when the parts are exactly the
  set the first declares. WP2 gets the whole count by reading each later part's header (its
  descriptors and three keys, 16 KB for Qwen3.6-27B BF16's second part, §2.2) and adding them;
  D3's reasoning for not reading them was the output head, and the count is a second use.
  `layer_bits_per_weight` stays part 1's own: for a dynamic quant that widens some layers (the
  `UD-` files mix widths by layer) part 1's width need not be the model's, so D7's floor and cap
  read it from every part's descriptors in WP2, or take part 1's as an estimate the card says is
  one. *(WP2 does both, D54:)* every part's descriptors when every part's header can be read,
  else part 1's width with a note that says it is an estimate. *(Refined in WP2's fix round:)*
  the later parts are read only as the set part 1 declares (D54).
- **D132 — A `416` over a `.part` that is the whole file finishes it.** When the CDN answers the
  Range with `416` and `Content-Range: bytes */N` where N is the `.part`'s length, the hub names
  the hash the `.part` was begun on, and its `x-linked-size` (when given) is N too, the `.part`'s
  sha256 (read before the request, D130) is checked and the file renamed into place, with nothing
  downloaded again. The ETag the update check compares is asked of the hub (`remote_etag`) when the
  `416` carries none. Before the review such a `.part` was dropped and downloaded again.
- **D133 — A paused download goes on by itself once the connection isn't metered.** While the
  boot's paused rows wait, `nmcli monitor` runs (`machine/netwatch.rs`, killed when the watch
  ends), and each change it reports asks the metered question again
  (`machine::connection_metered`); once the answer isn't "yes", every paused row nothing
  transfers goes on from its `.part`. The watch ends when they went on, when none waits any more,
  or when the monitor ends ("the paused downloads wait for Resume" in the log). Why NetworkManager's
  own events and no interval: an interval would be a number to guess, and the events are the
  moments the answer can change. The cost is one idle `nmcli` process while something is paused,
  none otherwise. Cancel on a row nothing transfers (paused, or cut and not queued again) ends it
  as a running transfer's cancel does, its `.part` deleted and its hash forgotten; the Downloads
  page offers it as "Cancel download" in the row's menu.
- **D134 — A download stops after 30 s without a byte, and says so.** The hub hop and every
  chunk of a transfer take the header read's bound (`hf::remote::STALL`, D44), each raced against
  the cancel, so a Cancel is seen while a transfer waits, not at its next chunk. The sentence is
  "Hugging Face sent nothing for 30 s while lmgw downloaded X; Retry goes on from the 4.2 GiB
  already here", and the `.part` stays. A slow line that keeps sending is never cut.
- **D135 — The Downloads page shows what a failed or cut transfer kept.** `DownloadRow` gains
  `kept_bytes`, the `.part`'s length for a row no transfer runs (failed, paused or cut), and the
  size column says "9.2 GB kept for Retry" (or "for Resume"), with a title that says the next
  transfer goes on from there.

**WP2's calls (2026-11-04, the build's own)**
- **D50 — The plan's one input reads the disk once, and the answer gains two keys.**
  `PlanFiles` (`modelinfo/plan_files.rs`) holds what the chat and aux plans read before: the
  weights' header, every part's bytes and the sentence for a missing one, the projector the rule
  picks and the first drafter with their headers (found by every sibling's header, as before),
  and the aux location rule's look into the aux dir. `PlanFiles::from_disk` makes all of those
  reads in one blocking task, and nothing after it touches the disk: the fit's sizing, its
  drafter header and the split-part check come from it. The plan's answer is unchanged but for
  two new keys, `source` and `fit` (D136). `tests/it/plan_pin.rs` holds the answers recorded
  before the move (a chat plan with two projectors and a drafter, a split one, an aux one with a
  projector and its tied ubatch, one without a card; since the fix round also a split file with
  a part missing, an embedder in the chat dir alone and with the same path in the aux dir, a
  non-causal decision plan on the CPU, and a plan that fits, all recorded from main at
  `0eed1eb0`) and compares every other key whole, the fixture's temporary dir as `<dir>` and the
  CPU's rationale up to the cores it names. Where the file lives is the input's too
  (`PlanFiles::found_in`, `in_aux_dir`).
- **D51 — A hub plan names the paths `hf_add` downloads to.** `gguf_path`, `mmproj_path` and
  `draft_gguf_path` are `<owner>/<name>/<file>`, so the hub plan of a file and the disk plan of
  the same bytes after the download are one plan (`source` aside), as §10 asks. Its class is
  `target` (chat or aux), else the header's (`class_for`); image and audio are refused with the
  catalog to use. `local_model_plan_of` takes either form and refuses both or neither, its
  messages naming the file's path without either surface's parameter name. *(Refined in WP2's
  fix round:)* the class is also where the file lives (`PlanFiles::found_in`): an embedder
  planned with `target=chat` gets the aux plan with the location warning, `apply_ready` false and
  the `hf_add … target=aux` to fetch it with, as the disk plan after `hf_add target=chat` does.
- **D52 — On the hub a companion is found by its name, and only the picked ones are read.** The
  disk plan reads every sibling's header to find the projector and the drafter; the hub plan
  can't afford that (1 to 16 MB a file), so it classifies siblings by name
  (`classify_repo_file`), picks the projector by D121 on the listing's sizes and the first
  drafter by path, and reads those two headers. When a picked file's header says it is something
  else, or can't be read, the plan names no such companion and says why in its warnings. The two
  ways agree wherever a file's name says what it is, which every repo of §4.4 did.
- **D53 — The fold's file is for a file the card shows that isn't the pick.** Under D7's rules
  the pick never fits a shorter conversation while a file of 4 to 8.5 bits keeps the whole one:
  rule 1 would have taken it. So `full_context_for(file, files)` names the smaller file that keeps
  the whole context for any file that fits shorter: the one the person chose (D34), or the
  whole-card pick shown under the beside one (D120). `FilePick.full_context` stays empty for the
  pick itself.
- **D54 — A split file's count and width are every part's.** Every later part's header is read,
  from the hub (16 KB each, cached by hash like any header) or the disk, and the parameter count
  and the bits per weight over the repeating layers add every part's own tensors up
  (`ModelSummary::layer_tensors`, `split_no`, `split_count`, `params::whole_layer_bits`,
  `whole_parameter_count_of`). When a part can't be read the count is none and the width is part
  1's, which the fit's notes call an estimate. *(Refined in WP2's fix round:)* the later parts
  are read only when every part is there (in the listing, or on disk) and part 1's `split.count`
  is the count its name says (`plan_files::set_mismatch`): llama.cpp loads the parts the header
  declares, so a set that disagrees doesn't load as it is. Then no later part is read, the
  plan's warnings say why, and the fit is by size (`PlanFiles::size_only`), `unknown` when a
  part's bytes are.
- **D55 — Beside what runs charges every model of lmgw's on the card, starts under way
  included.** Each resident is its charge plus the image peak kept free for it, as `crowded`
  adds a victim up; an audio model's pending load is part of its charge already and isn't added
  again. A reservation (a start admitted whose container isn't up) counts: its bytes are about to
  be on the card. Outside use is `outside_of`'s; without a device reading it is unknown, the
  measure is the card less lmgw's models alone, and the note says so. *(Refined in WP2's fix
  round:)* a resident whose row serves the planned file (a chat or aux row on the same path in
  that class's dir) is left out, and the note names it: a start of the planned row takes its
  place, so a re-plan of a loaded model is never fitted beside itself.
- **D56 — A file without a width is never the pick.** A size-only mark (D8: the header not read
  or unreadable) compares the listed bytes plus the headroom with the room. A header without
  `blk.*` tensors has no width either. Neither can be checked against the floor and the cap, so
  neither is recommended; both keep their marks in the list.
- **D57 — A plan on the CPU fits with nothing on the card; a plan without a context fits by its
  weights.** The decision plan can propose the CPU (`backend: cpu`): its fit is "fits" with no
  bytes and a note. A header that states no context and a plan that sets none are fitted by the
  weights alone, the footprint's note saying the cache wasn't sized. The CPU's "fits" holds with no
  card to measure against too: it needs none.
- **D58 — The download rate counts what each transfer moved, every kept job.** Every finished
  `hf_download` job the job table keeps counts (the table is bounded by its own retention
  setting, so there is no second count), each its `bytes` less its `resumed_from`, over
  `finished_at` less `started_at`, less the read of a resumed `.part` that checked its hash
  (D130; the job's result records it as `check_secs`). The table times in whole seconds, so a
  download under one second has no time and is left out. The header reads' fallback is their received bytes over the
  time from the CDN's answer to the stream's drop (`RemoteHeader::read_secs`). The engine is
  `hf::rate::engine`: Podman's presence, then the registry manifest's layer sizes for this
  platform; when either can't be told, the time leaves it out and says so. The time rounds up to
  the minute, and under a minute reads "less than a minute on your connection".
- **D59 — A plan from the hub is a request, so it reads on a metered connection.** The plan op,
  its tool and `hub_fit::file_fit` read the headers they are asked for, as a Resume pressed on a
  metered connection goes ahead (D47). Not reading by itself on a metered connection is the
  page's rule (D123, WP6), which asks for the reads.
- **D136 — Every plan answers its fit, by the plan's own numbers.** `fit::fit` searches the
  footprints `plan_fit::Sizer` gives over the plan's input and its own parameters with the
  plan's warning's search (`largest_fitting`), so a fit and the warning agree to the token. The
  answer's `fit` is `PlanFit`: the card measured (`CardMeasure`) and the fit by both measures.
  A split set that can't load as it is (D54) is the one plan whose fit is by size instead of the
  search; its warning, if any, still sizes the parts there are. `tests/it/plan_pin.rs` holds
  every pinned plan's fit against its warning.
- **D137 — The wire types differ from §9.2's sketch in four places.** `FitMark` is flat
  (`fits`, `fits_shorter`, `does_not_fit`, `fits_by_size`, `does_not_fit_by_size`, `unknown`,
  `no_card`) rather than a `BySizeOnly { fits }` variant, so every mark is one string; `FileFit`
  gains `headroom_bytes` and `parameters`; bits per weight are `f64`, as the header's are; and
  `PlanResult` gains `source` and `fit`. They live in `lmgw-api-types/src/model_find.rs`. An
  image plan carries neither yet (its fit on the card isn't measured), so `PlanResult.fit` is
  an `Option`, and the route's text and the tool's description say so.
- **D138 — The estimate from another file's header is WP2's; which files get read is WP3's.**
  `PlanFiles::estimated` plans another file of the same model from one header with its own
  bytes charged whole (not less the host-kept input embeddings). A test over a repo of three
  widths with untied embeddings holds it never below the exact footprint. The band of files
  read exactly near the card's edge (§3.3) is built with the file list, where the reads happen.
- **D139 — D26's data is WP2's, the row's note is WP5's.** `fit::fitted_params` gives the plan's
  parameters at a fit's context, moving a tied ubatch and batch with it, and `fit::note` the
  `FitNote` a row keeps (the context, the trained one, the measure, its bytes, when). Nothing
  writes either yet: Get it (WP5) and the editor's "Fit again" are the writers.
- **D140 — The width D7 caps is the quantized weight matrices'** (WP2's fix round, from its
  review's blocker). D7 measures bits per weight over the repeating layers, the `blk.*` tensors.
  Taken over every one of them, a real Q8_0 read wider than Q8_0: llama.cpp's quantizer leaves
  every 1-D tensor (each layer's norms and biases) at F32, and alignment padding counted from the
  offsets added more, so `bge-small-en-v1.5-q8_0.gguf` read 8.566 bits and its Q8_0 was never the
  pick, the opposite of "a small model that fits in BF16 or F32 gets its Q8_0". So the width is
  measured over the `blk.*` tensors of two or more dimensions (the only ones the quantizer
  touches, `tensor_allows_quantization`), less those it keeps at F32 though 2-D (MoE gating,
  Mamba's conv1d), unless every one is F32 (an F32 file, 32 bits); and each tensor's bytes are
  its ggml type's size over its elements, from `gguf-py`'s `GGML_QUANT_SIZES` table, without the
  padding (the offsets' bytes for a type the table doesn't know, or when they are fewer). The
  Q8_0 reads 8.5 exactly. The header reader now keeps each tensor's rank and type
  (`GgufMeta::tensor_ranks`, `tensor_types`).

**WP3's calls (2026-11-04, the build's own)**
- **D60 — The one header read first is the file most likely to be the pick.** §3.3 reads "the
  recommended candidate's header" before anything says which file that is. So the order is the
  listing's bytes: the largest file whose bytes plus the headroom fit beside what runs now, else
  the largest that fits the whole card, else the smallest file; when a read fails the next in that
  order is read, and the failed file keeps its mark by size (D8). A file the listing gives no
  bytes for is never read: its mark is `unknown`. No card to measure against (D9) reads nothing at
  all, and every mark is `no_card`. Built in `modelinfo/hub_fit/marks.rs`.
- **D61 — The band's edge is what a file's own input embeddings could weigh at most.** §3.3 takes
  "the reference's host-kept input embeddings" off the estimate, but those differ by quant: a
  Q4_K_M reference's are about half a Q8_0's, so a Q8_0 that fits by its own header could stay
  "doesn't fit" by the estimate and never be read. The bound taken off instead is the same
  tensors' elements (`ModelSummary::host_input_elements`, the same in every quant) at 32 bits, F32
  being the widest type a converter writes a weight in, and never more than the file. That bound
  is a true floor of the file's footprint, so a file is read exactly when its estimate's mark and
  the floor's differ by either measure, and only then: the marks are every header's marks. A model
  whose head is tied keeps only its per-layer input embeddings on the host
  (`per_layer_token_embd.weight`, Gemma 4's E-models), so its band is those tensors' at 32 bits,
  and empty for a model without them; an aux plan's is empty too, as it charges the embeddings to
  the card. *(Corrected in WP3's fix round:)* the elements are every part's of a split file
  (`gguf::InputTensors::host_elements` over the parts `Whole::of` read, D54), so a split
  reference whose untied head sits in a later part bounds its siblings by its input embeddings,
  not by none; with the later parts unread they are `token_embd.weight`'s whatever the head, and
  a count that isn't known (a dimension that overflows) bounds by the file's bytes.
  The band is wider than §3.3's (by the 32 bits): for a 5120-wide model with a 152k vocabulary,
  about 3 GB of file size around the card's edge. A shorter conversation's number from an
  estimate is a floor and reads "fits · at least 65 536 of 262 144 tokens"; a file is not read
  for its number alone.
- **D62 — The pick's reads go on until no estimate could change it.** An estimated file has no
  width and can't be the pick (D56). So while an estimated file's floor fits under a rule the pick
  doesn't meet (the whole context over a shorter one), or under the same rule while the file is
  larger, the largest such file is read and the pick taken again, by both measures. A file whose
  bytes over the model's quantized layer elements are under 4 bits can't reach D7's floor and
  isn't read for the pick (`PlanFiles::layer_elements`, every part's). The pick is then the one
  every header would give, and so is the fold's file (D53). The cost: for a small model whose BF16
  and F32 files fit, those headers are read on the way down to the Q8_0, one each, unless D67's
  bound from above rules them out by their bytes. Measured on a dev instance against the hub (2026-11-04, no token, a 24 GiB test GPU with about 2 GiB held by other programs): `unsloth/Qwen3-32B-GGUF` read 13 of its 26 headers,
  72.7 MB in 12.5 s (the 32-bit bound of D61 puts most of the 15 to 22 GB files in reach of the
  pick); `unsloth/Qwen3-8B-GGUF` read 18.2 MB. A 16-bit bound would read fewer, but nothing
  says an embedding is never F32 in a quantized file.
- **D63 — The wire differs from §9.2's sketch in three places.** `RepoFile.fit` is a `PlanFit`
  (the card and both measures, exactly what `local_model_plan` with `repo` + `file` answers as
  its `fit`) rather than one `FileFit`, so the list can say "fits once other programs close"
  (D120), and an exact read on click replaces it whole. `RepoFile` gains `estimated_from` (the
  file whose header the fit was estimated from). `RepoFiles` gains `picks` (`RepoPicks`, the pick
  by each measure with its "Why this file" line) and `fit_notes` (the hold's line, or why nothing
  was measured) beside `recommended`, `card` and `header_bytes_read`. `recommended` is the beside
  pick, else the whole card's (D120). Only `weights` files get a fit; the image and audio classes
  read nothing and say why in `fit_notes`. The new keys are in every answer, `null`, `0` or empty
  without `fit`. *(Revised in WP3's fix round, from its review's finding 10:)* the answer says
  once what every file would repeat: a file's `fit.card` is `null` where it is the list's `card`
  (it stays for a file that a loaded row serves, measured without that row, D55), and its notes
  leave out what `fit_notes` says once (the hold, admission off, what other programs use not
  being known, where the estimates came from, why the reads stopped). The estimate's sentence no
  longer rides on every estimated file; `estimated_from` names the file and `fit_notes` says what
  an estimate is. `hub_fit::marks::compacted` is the one rule, and the page puts the list's card
  back on a file's fit before it reads it. `unsloth/Qwen3-32B-GGUF`'s answer was 33.5 KB before.
  `RepoFiles.metered` is D68's.
- **D64 — The marks' words.** The chip leads with the beside measure (D120): "fits · 262 144
  tokens", "fits · 65 536 of 262 144 tokens", "fits by size · not checked", "size unknown · not
  checked", "doesn't fit"; a file that fits only the whole card reads "fits once other programs
  close", "fits once lmgw's other models unload", or both joined, by what is on the card. Its
  tooltip has the footprint's terms ("Uses 21.0 GB of the 24.0 GB free beside what runs now:
  weights 16.0 GB, the conversation 4.0 GB, working memory 1.0 GB; 512 MB kept free"), what is
  beside it, "How long a conversation it can remember · context, …" (D39), the bits per weight,
  an estimate's source and the fit's notes. A "recommended" chip sits beside the pick's mark, and
  "checking…" on a file being read. Above the list: "Recommended: <file>. <why>", the fold's
  sentence, the whole card's pick when it differs ("With the other programs closed and lmgw's
  other models unloaded: <file>."), the hold's line, and "lmgw read 11.0 MB of file headers to
  check which fit" in the count line. Without a card there are no marks, and the line says why.
  *(Revised in WP3's fix round, from its review's findings 5 and 6:)* when the recommended file
  is the whole card's pick because nothing qualifies beside what runs now, its chip leads with
  the whole card too, "fits · 39 168 of 40 960 tokens once other programs close", its tooltip
  adding what it fits beside what runs now, and the line reads "Recommended once other programs
  close: <file>. <why>", then the beside pick's line saying why none qualifies there; every other
  file's chip leads with the beside measure as before. The line for a beside pick that differs
  from the whole card's reads "Once <who>: <file>.". Who has to make room is what the card says:
  other programs only when the card reads some outside use, lmgw's models when some are on it,
  and with outside use unknown and lmgw's models on the card "once lmgw's other models unload"
  (`marks::who`, `marks::list_lines`).
- **D65 — The file list lists first and marks after.** `HfWizard` asks `hf_repo` without `fit`
  (the list in about a second), then with it for the chat and aux classes; meanwhile "Checking
  which files fit your graphics card…" and the weights files' "checking…". When the marks come in
  and nothing is chosen yet, the recommended file is selected: nothing downloads before the
  button. Choosing a file whose mark is an estimate, by size, or not in yet reads it exactly
  (`local_model_plan` with `repo` + `file`), and the button waits for that read; a read that
  fails enables it with the reason in the footer, since every file stays choosable (D8). Its
  props: `initial_repo` (listed at once), `initial_file` (selected in place of the recommended
  one), `initial_target`, and `on_pick: Callback<PickedFile>`, the pick-only mode (D34), whose
  button reads "Use this file" and hands back the repo, the file, the class, the companions box
  and the file's fit; the class switch and the companions box stay, as D34's "every file and
  class, the companions box" says. A host without the Models page's reload context can mount it.
  *(Revised in WP3's fix round, from its review's findings 7 and 8:)* the page asks
  `fit=unless_metered` (D68), and a file chosen before the marks are in (a click, or
  `initial_file`) is read only once their answer says there is a card, the connection isn't
  metered and the file's mark isn't a header's; so no read runs beside the marks' own, and none
  without a card (D9). A list whose marks failed reads the file chosen. A new Fetch clears the
  list, the marks, the "checking…" state and the selection before it asks, so a failed listing
  shows nothing of the repo before it.

- **D66 — One answer's reads share one card and one set of companions, and stop at the hub's
  refusal** (WP3's fix round, its review's findings 1, 3, 9 and 12). *One card:* the card is read
  once per answer and every file, estimated or read exactly, is measured on that reading
  (`hub_fit::file_fit_planned` takes it, `plan_from_on`), so the marks of one answer compare
  against one card; `file_fit` alone still reads the card itself. *Companions:* an estimate
  carries its reference's projector and drafter, and the hub plan finds them per directory, so a
  file is estimated only from a reference with the same companions: a file of a directory that
  holds a projector or a drafter by name from a file of that directory, a file of a directory
  holding neither from a file of any directory holding neither (`marks::companion_key`). Each such
  set reads its own reference (D60's order within it). Rebuilding each file's companions from the
  listing was the other way; it would read every directory's companion headers to estimate, where
  this reads one weights header per set. *Refusals:* the header read's error is typed
  (`hf::ReadError`): a 401 or 403 (gated, a rejected token, no access), a 429, a 5xx and no
  answer at all (the connection, or nothing for 30 s before an answer) are the hub's and stop
  every read of the answer there; a 404, a cut stream, a body that isn't a GGUF and a header past
  the memory bound are the file's, which is marked by its size while the reads go on. A plan whose
  projector, drafter or later part meets the hub's refusal fails as a whole now rather than
  planning without it. After a stop the files keep the marks they have (an estimate stays true),
  nothing is recommended ("Hugging Face stopped the header reads (<why>), so a file not read could
  still be the one"), and `fit_notes` says why. *Counting:* `header_bytes_read` adds the bytes a
  failed read received before it failed (the error carries them), so a cut stream's bytes count.
- **D67 — The read band's bounds, tightened where it is safe** (WP3's fix round, its review's
  proposals). *The pick side:* a candidate holds at least D7's 4 bits for each of the model's
  quantized layer elements L, which its host-kept embeddings aren't part of, so for the pick's
  reads the most those can weigh is also the file's bytes less L/2 (`bounds::host_bound_for_pick`);
  the marks' band keeps D61's bound, as a file under 4 bits is still marked. *The cap side:* a
  file's layers hold at least its bytes less every other parameter at 32 bits (P − L, F32 being
  the widest) and less its headers and padding, taken at twice the reference's own headers plus
  their tensors' alignment padding (the same model's headers differ by a few keys), so a file
  whose layers come to more than 8.5 bits by that measure can't be the pick and isn't read for it
  (`bounds::may_be_under_cap`; the twice is this build's margin, not a measured one). *The order:*
  within D60's tiers the reference is tried among the files whose label names 4 to 8 bits first
  (`bounds::label_bits`), which changes only the order. Not taken: assuming every file has the
  reference's embedding type, which `_L`, `_XL` and UD files break. Measured on the real hub
  (2026-11-04, no token, a dev instance, a 24 GiB test GPU with about 2 GiB held by other programs): see the WP3 entry;
  on `unsloth/Qwen3-32B-GGUF` every read left is the band's, so these saved none there.
- **D68 — On a metered connection the page reads no header by itself** (D123, WP3's fix round,
  its review's finding 4). `hf_repo`'s route takes `fit=unless_metered` beside `true` and
  `false` (`RepoFit`): the marks as `true` gives them, except that when NetworkManager calls the
  connection metered (`machine::connection_metered`) nothing is read, every weights file is
  marked by its listed size against the card, nothing is recommended, and `RepoFiles.metered`
  says why ("The connection is metered (eth0: yes), so lmgw read no file's header…"). `fit=true`
  and the MCP tool's `fit` read on a metered connection too: an ask is an ask (D59). The check is
  the server's, as the metered fact is; the page asks `unless_metered` and shows "Check exactly"
  under the line, "reads the start of the files it needs, a few MB each": lmgw has no figure for a
  repo before reading it, so the words name no number.
- **D69 — Band files marked "near the edge" instead of read. Ruled by the owner 2026-11-04
  (ruling 1): not wanted; the band's files stay read up front, as WP3 built it.** The largest
  share of the reads left is the band (D61): every file whose estimate and floor differ is read up
  front, 12 of the 13 reads of `unsloth/Qwen3-32B-GGUF` on the measured card. Such a file could
  instead keep its estimate's mark with "near the edge · checked when chosen" and be read when
  chosen (D65 reads a chosen estimate anyway). Its cost: those files' marks would be ranges, not
  the header's marks, so "every mark is the header's" (§3.3's promise) would hold only for the
  files read, and a file that does fit would read "near the edge" until chosen; the pick's reads
  (D62, D67) still run, so the recommended file stays every header's. Not built, as ruled.

**WP4's calls (2026-11-04, the build's own)**
- **D70 — Each listing is one page at the hub's own page size.** The two searches and the uploads
  listing send no `limit`, so the hub's page (1000 repos today) is what gets ranked, and lmgw adds
  no bound of its own. §4.4's spike asked for 100: a whole page of "mistral small" is 600 KB, not
  50 KB (about 150 KB compressed; lmgw's HTTP client doesn't ask for compression yet). A search's
  next page is `next_cursor` (its two cursors in one string), and asking with it answers `more`:
  that page's originals and partial matches, no card. The uploads listing's next page isn't
  followed: the card's upload is the first of its order, so the card says `more_uploads` instead.
  *(Review round.)* Still no `limit`: an explicit maximum would pin the page if the hub's default
  ever changes, and the hub's documented maximum hasn't been read live. The hub's answers now
  come compressed (D141).
- **D71 — Without a header, a file's width is its bytes over the listing's count, and the Q8
  class ends halfway to 16 bits.** `model_find` reads no header (D123), so the walk's file pick
  is by size: the largest file of 4 bits per weight or more, whose whole-file width is below
  12.25 bits (`WHOLE_FILE_CAP_BITS`, halfway between Q8_0's 8.5 and the 16 of F16 and BF16),
  that fits with the headroom. A whole file carries its tokenizer, norms and embeddings too, so
  a real Q8_0 reads a little over 8.5 (a 0.6B embedder's about 8.6), and no GGUF weight type lies
  between the two. Without a listing count every file is in the band and the header's count caps
  it later (§4.4's bge m3). The header pick of D7 replaces this pick when the page reads it.
- **D72 — The walk measures against the whole card; the card shows both picks.** An original
  "has a file that fits" (D17) when its file fits the whole card, since admission unloads lmgw's
  other models for it (D5). The card's file is then the beside pick when one fits, with the
  whole-card pick in `whole_card` when the two differ (D120), else the whole-card pick with "fits
  once what else is on the graphics card now is closed or unloaded". Without a card (D9) nothing
  is passed over, the pick is the largest file in the band and the marks say `no_card`.
- **D73 — D20 as an order.** An original's plain uploads are ordered open before gated, then the
  maker's own (D14's folded GGUF or an upload by the same author), then likes, then downloads. A
  gated upload whose tree answers 401 or 403 is passed for the next; when every upload was, the
  answer is `all_gated`, naming the first one's repo.
- **D74 — Two answers more, and a flat enum.** `FindAnswer` gains `other_kind` (the top original's
  pipeline is D30's: the sentence points to Models → Add from recipe and the audio catalog) and
  `more` (D70). It is flat, as `FitMark` is (D137): `rate_limited`'s reset is in `hub_budget`,
  `not_hugging_face`'s host in the sentence. `loose_only`'s sentence ends with D18's digits hint
  too, as every answer that found nothing does.
- **D75 — The card's wire type differs from §9.2's sketch.** `download` is WP2's
  `DownloadEstimate` (bytes, engine, rate and its source, seconds, sentence, notes) in place of
  three fields; `whole_card` is a `CardFile` (the file, its quant, bytes and fit), since the other
  measure's pick is another file; `quant` and `more_uploads` are added; `parameters` is the
  listing's count until the header's replaces it; `licence` is the `ModelLicence` the other views
  use, its id from the hub's `license:` tag and its link the original's page (D22). `ModelFind`
  gains `notes`, the walk's steps (what was passed over and why) and what couldn't be read.
- **D76 — The budget is the hub's, and its last answer decides.** `Reads` keeps `RateLimit`'s `r`
  and `t` and `RateLimit-Policy`'s `q` and `w` from every answer. When an answer says no request
  is left, the next read isn't sent and the answer is `rate_limited`. `hub_budget.near` is set
  when fewer requests are left than this answer made, with the sentence "Hugging Face's limit
  resets in 40 seconds: 2 requests are left, and this search took 6." and the policy's allowance.
- **D77 — An address goes straight to its repo.** A GGUF-only repo is the upload, and its one
  quantized base (one more read) the original; any other repo is the original. A named file is
  the pick when it fits, else the same upload's size pick with a note that says so (§7). An
  address is never passed over by its count: the card shows the file it names.
- **D78 — The recording is the finder's own reads.** `scripts/hf-find-record.sh` runs the
  ignored test `hf_find::regression::record_the_queries`, which drives `find::find` over the live
  hub through a recording `Hub` with no token, on both cards, and writes what it was answered:
  `tests/fixtures/hf_find/index.json` (each read's path and status, readable) and `answers.zip`
  (each answer's status, rate-limit headers, `Link` and body, deflated). A listing's repos are
  kept as the finder's reader reads them: `HubModel`'s fields and the tags `tag_read` names
  (`conversational`, `pytorch`, `gguf`, `license:…`), which `HubModel::has_tag` is held to by a
  debug assertion. Whole, the pages of D70 made a 4.3 MB archive; trimmed, about 1.5 MB. A rule
  that asks the hub something new fails the regression test naming the read and the script.
- **D79 — The card's extras never fail the card.** A display name that can't be read (D19) leaves
  the author's id with a note; the engine's check and the download rate are read only for a card
  with a file, since the engine's check asks Podman and maybe a registry. *(Review round.)* So
  does the original of a pasted GGUF repo: when its listing can't be read, the upload is its own
  original, with a note.

**WP4's review round (2026-11-04, the build's own)**
- **D71, revised in the review round — an upload with no file in the band is passed.** Like an
  upload without weights, it is passed for the next upload, with a note naming what its files
  are; when no upload of the walk has a file in the band, the answer is `no_file_in_band`, its
  sentence saying what the files are (narrower than 4 bits per weight, or the full 16- or 32-bit
  weights, or split into parts the listing doesn't give in full) and the band in plain words:
  "lmgw picks a file of 4 to 8 bits per weight … a file under 12.25 bits per weight by its size
  still counts as 8-bit, since a whole file carries more than the weights." The notes' old "4 to
  8.5 bits" said the cap of D7's header width, not this pick's.
- **D73, revised in the review round — one upload's error is that upload's.** A tree that
  answers any status but 401, 403 or 429 (a 404, a 451, a 5xx) passes the upload with a note,
  and an uploads listing that does passes the original. When every upload the walk asked about
  answered so, the answer is `unreachable` with its own sentence ("Hugging Face answered, but
  not when lmgw asked for the files of …"). Only a search, or an address's own
  listing, that fails is "can't reach Hugging Face". A gated upload now leaves a note too.
- **D76, revised in the review round — a limit used up partway keeps what was found.** When the
  hub's limit runs out during the walk (an answer said none are left, or a 429), the walk stops
  there: the closest original found too big is the `nothing_fits` card, else the answer is
  `rate_limited`; either way the lists stay, and a note names the limit, how many of the walk's
  originals were checked, and when it resets.
- **D77, revised in the review round — what an address names.** A file of an original's own repo
  that isn't a GGUF (its `config.json`, a safetensors file) leaves a note and walks the
  original's uploads as its name would, never passed over by its count. A named file not on
  `main`'s tree, or not a weights file, leaves a note naming it (and the revision the address
  names, which lmgw doesn't read). A named file that fits the whole card is the card's file
  however narrow, and when it doesn't fit beside what runs now it is shown on the whole card with
  D72's "fits once what else is on the graphics card now is closed or unloaded", never swapped
  for another file. A plain `owner/repo` or repo address is never passed over by its count
  either.
- **D19, refined in WP4's review round — a person's name with the account.** A user's
  `fullname` is self-chosen, so the card carries `by_user`, the account's id, beside `by`, and
  the sentence says both ("Demo by Jane Example (jane-demo)."). An organisation's name is shown
  alone.
- **D31, refined in WP4's review round — the share link is encoded.** Each segment of the upload
  and the file is percent-encoded (`share_url`), so a space, `#` or `?` in a file's name survives
  and the Find box reads the link back as the same file.
- **D78, revised in the review round — a new field asks for a new recording.** A listing's repos
  are kept as `HubModel` writes them, every field present. Loading the recording checks every
  kept repo has every field `HubModel` has now; a field the finder starts reading fails it,
  naming the field and `scripts/hf-find-record.sh`, rather than defaulting silently.
- **D141 — The hub's metadata reads come compressed, and nothing else does. Ruled by the owner
  2026-11-04 (ruling 7): gzip stays off for the upstream LLM requests.** lmgw-core's reqwest
  has `gzip`, which is process-wide, so every client but one opts out with `no_gzip`: `state.http`
  (every upstream request and stream), `proxy_http`, the OCI and forge clients, the Podman socket
  client, an MCP server's streamable-HTTP transport and quickdoc-core's embedder (which takes the
  feature only to opt out). The reason: no unverified change to an upstream's requests and streams
  (a gzip decoder in a stream's path, an upstream's behaviour with `Accept-Encoding: gzip`); the
  route goldens of `tests/fixtures/egress_routes` stay as they were. The one client that asks for
  gzip and decodes it is `AppState::hub_http`, with `http`'s connect timeout and TLS, used for the
  hub's search, listings, trees, uploads and overviews (`LiveHub`, `hf::list_tree` and
  `list_repo_files`, the audio package listings). `hf::get_with_commit` (a file's hops and the
  header's `Range` reads) keeps the non-gzip clients and asks every hop for `Accept-Encoding:
  identity`, because a decoded answer loses its `Content-Length` (the download's and the header
  read's measure) and because reqwest 0.13.4 asks for gzip on a `Range` request too (its
  `Decompression` layer adds `Accept-Encoding` whenever none is set; the builder's doc comment
  about `Range` predates it). brotli isn't on yet: gzip gives most of the gain, and brotli would
  compile the `brotli` crate into lmgw-core. *Not yet:* gzip for upstream LLM requests, which
  needs a streaming-latency check first.
- **D142 — The answer's lists come in windows.** A page of the hub can hold hundreds of
  originals, and an uploads listing hundreds of uploads, and an MCP answer is read whole into a
  model's context: over the recording, an answer with every entry was 3 to 202 KB ("llama"
  202 KB, "qwen" 155 KB), and with only `others` and `did_you_mean` cut, the card's
  `other_uploads` still made "qwen" 88 KB (83 KB of it uploads). `model_find` takes
  `others_offset`, `others_count`, `did_you_mean_offset`, `did_you_mean_count`,
  `other_uploads_offset` and `other_uploads_count` (0 and 10 by default, `DEFAULT_LIST_COUNT`),
  and every answer carries each list's `*_total` and `*_more`, so nothing is hidden. With the
  defaults the recording's answers are 3 to 9 KB (measured without a card's rate, engine or disk
  notes). Another window runs the same search and walk again (the hub's calls again), so the
  card comes back with it; the page (WP6) asks for more.

**WP5's calls (2026-11-04, the build's own)**
- **D80 — The intent and the fit notes are tables of their own** (migration 0098). `model_gets`
  holds a get (repo, file, class, measure, model id, its download ids, status, the row it set
  up), one running per file by a partial unique index. `model_fit_notes` holds a row's
  `FitNote` by class and row id, and triggers drop a note with its row and with any change of
  the row's context that isn't Fit again's own, so a note only stands while the context it
  records does. Why not a column on the rows: the row types are built in some 200 places, and
  a note that outlives a hand-edited context would say something false. The wire types are
  `lmgw-api-types/src/model_get.rs`, not `model_find.rs`, so WP5's file stays apart from WP3's
  and WP4's; `ModelGetStarted` gains `get_id`, `measure`, `repo`, `file`, `companions`, `disk`,
  `joined` and `message` over §9.2's sketch.
  - *D80, revised in the review round.* The op writes the `model_gets` row first, with no
    downloads, then queues the files and records them on it (`download_ids`, and `queued_ids`
    for the ones it started itself), then starts the job, and everything from the row's write on
    runs as a task of its own (`crate::detached`): a dropped request can't leave downloads with
    no get or a get with no job. A get the process cut before its downloads were recorded fails
    at its next start with "lmgw stopped before the downloads of <file> were queued; Get it
    again". The row also keeps the aux kind its header said (`kind`, so a joined get answers
    `decide/` for a decision model), the note its row will take (`note`, recorded before the
    create) and its job (`job_id`). Migration 0098 was amended in place (not released).
- **D81 — A get waits on the jobs feed, not on an interval.** It reads its download rows again
  on every frame of the jobs feed (a transfer's progress, start, end), so it moves when they
  move and costs nothing while nothing does; a cancel of a job now publishes a frame too
  (`jobs::cancel`), so a get waiting on a paused download sees its own cancel at once. A
  download that fails fails the get with "<file> didn't download: <its sentence>"; a cancelled
  one cancels the get; a cancelled get cancels its downloads still on their way. A row waiting
  with no transfer (paused on a metered connection, cut and not queued again) makes the stage
  `paused` with that row's sentence.
  - *D81, revised in the review round.* Every `hf_models` status write made outside a transfer
    publishes a jobs frame of its own (`JobManager::publish`: a Cancel or an untrack of a paused
    row, the boot's metered pause, the update check's flag, the audio install), so a get waiting
    on such a row sees it; a change of the get's stage or of why it waits goes out at once
    (`progress_now`), not through the throttle. A get's cancel stops only the downloads it started
    itself and that no other running get lists: a projector shared by two gets, or a file a
    manual `hf_add` is fetching, goes on with its `.part`. A failed download fails the get with
    the download's sentence, its "Retry" said as "Get it again", which queues again only what
    isn't here and sets the model up after (Retry on the Downloads page finishes the download
    alone); a download sentence without it gets "Get it again to try once more". Of the two ways
    the review offered, this one, since the get then never waits on something only a person can
    move. A shutdown leaves the get `running` for the boot, as its sentence says. A job of the
    file already running is the get's when its input names it; an earlier get's (ended, its job
    a moment from its end) is waited out once and the start tried again, else the start fails:
    a get never takes a job that runs another.
- **D82 — Get it fetches `hf_add`'s files, less what is on disk.** `hf_add`'s companion choice
  is one function now (`ops::with_companions`: the file, the projector D121 picks, and for chat
  the drafters), and `model_get` calls it. A file whose every part has a `done` download row
  and is on disk isn't queued again: it is listed with its row as `done`. Any other file is
  queued whole, as `hf_add` queues it.
- **D83 — With no `target`, the class is the header's, read before the download.** The weights'
  header is read from the hub (cached by its hash, so after the card's read it costs nothing),
  and also for `target=aux`, whose public name depends on the kind (`embed/`, `decide/`).
  `target=chat` reads nothing. The image and audio classes are refused with their catalogs.
- **D84 — The id when none is given is the repo's name, less a trailing `-GGUF`.** `model_get`
  receives the upload, not the original, so D28's "original's repo name" comes from the card,
  which passes it as `model_id` (WP6); without one the upload's name lower-cased, less a
  trailing `-gguf`, `_gguf` or `.gguf`, is the generic rule. Taken is: a row of the class with
  that id, a running get that will create it, or its public name already served on `/v1`
  (`candidate_alias_name_taken`: a plain or candidate alias, any class's row, case aside).
  The id is settled again at create time against the names taken by then.
  - *D84, revised in the review round.* Names an upstream passes through count too: a local row
    wins a name's resolve, so a row named like a pass-through model would take its requests. A
    public name under an `expose_all` upstream's prefix is refused (no suffix gets out from under
    it), and so is a given `model_id` that a bare `expose_all` upstream lists now; a derived one
    it lists gets the suffix. The refusal names the upstream. A bare upstream whose catalog can't
    be read is logged and checked against nothing.
- **D85 — The get's measure decides, with no fallback to the other.** The plan's fit by the
  measure the card showed: `fits` keeps the plan's context and writes no note; `fits_shorter`
  takes the largest that fits with its note; `does_not_fit` and `no_card` create the row at the
  plan's context with the plan's own warning in the result and no note, as the planner does
  without K (D35). A plan that can't be applied as it is (an embedder asked as chat, D51) fails
  the get with the plan's warnings.
- **D86 — The aux path re-plans until a plan fits whole** (D26 as ruled). The aux plan is made
  again at the fitted context (its header's trained context read as that one), so the encoder
  rule's ubatch and batch, and a projector's ubatch floor, are judged at it; the re-plan is
  sized again, and when its own figures fit less, planned again at what they fit. Each round
  plans below the last one, so it ends. A chat row's context moves alone
  (`fit::fitted_params`). The note keeps the model's trained context, not the re-plan's.
  - *D86, revised in the review round.* A projector's floor above the fitted context doesn't
    take a second round with today's sizer: the compute buffer is charged at the ubatch the
    context clamps it to (`vram/compute.rs:149`), so the floor costs nothing more and the
    second sizing confirms the first context (a test holds it). The loop stays for a sizer that
    charges more. The row keeps the floor; Fit again keeps an aux ubatch and batch above the
    context (a floor) until the context grows past it, and moves one equal to the context with
    it. A chat row's context moves alone, its batches where its plan put them.
- **D87 — A row that serves the file is the answer.** At the create step the get answers the
  row it recorded before (`model_gets.row_id`), else a row of the class with its model id on
  that path, else any row on that path, and creates nothing (`created: false`). This is what
  makes a get re-attached after a restart between its create and its end answer the row it
  made, without a second one; the plan's `configured_as` isn't consulted, since a row Get it
  fitted shorter is never "in sync" with a plan at the trained context.
  - *D87, revised in the review round.* The op looks first: a row of the class on the file,
    with the file on disk, is the answer up front (`row_id`, its own `model_id` and public name,
    no `get_id`, no job, nothing queued), so the answer's id is always the row's. The job still
    looks before it plans (a restart after the create), and a row this get created (its
    recorded row, or its model id on that path) gets the note the get recorded for it when the
    restart cut it off before the note's write, while the row's context is still the note's. An
    MCP caller reads a get's outcome with `model_get_status` (`lmgw__model_get_status`, a read
    op by `get_id`): the state, a running get's stage, why it waits and its bytes, each file's
    download row, a failed get's sentence and a finished get's result with the row. A second
    `model_get` of the file was the other way offered; a read by id says a failure's reason,
    which a second get would replace with a new attempt.
- **D88 — Fit again sizes the row as it is, searched up to the trained context.** The row's own
  parameters (a chat row's whole `LlamaParams`, an aux row's context, batches, projector and
  CPU placement) are fitted with the context set to the trained one, against the card now with
  the row left out beside it (`CardNow::beside_itself`); the measure is the note's, else beside
  what runs. It changes the context, and an ubatch and a batch equal to the row's context; with
  the whole context fitting, the note goes. It is the op `model_fit_again` (`confirm` is the
  change list as shown, as `machine_settings_fit` confirms; otherwise `outdated`) with no MCP
  tool yet. The editors (chat and aux) show the note and "Fit again" for a row that has a note.
  - *D88, revised in the review round.* A confirm saves the list as shown when the context it
    sets still fits the card now (the largest that fits is at least that) and the list is what
    the row's parameters as they are now make of that context; else `outdated`, saying which
    ("the model's settings changed", "that context no longer fits"). Not the token-exact match
    of before: beside what runs reads the card's live bytes, which move a little all the time.
    Only an aux row's ubatch and batch are tied to its context (the encoder rule, D29); a chat
    row's batches stay, in Fit again and in Get it's fit alike. The save and the note's write
    are one task of their own, so a dropped request can't leave a new context without its note.
    The note is on the row's read (`fit_note` on `LocalModelDetail` and `AuxModelDetail`), which
    the editors read it from, so showing it costs no write op; the editor's own save reads the
    row again, and a save of another context drops the note there. The change lines use the
    editor's labels (Context size, Physical batch, Batch size).
- **D89 — The free-disk check is the files' listed bytes less what their `.part`s hold, against
  `statvfs` of the models dir now.** Files already on disk count nothing. The engine image isn't
  counted: Podman's storage can be on another disk, and the answer's notes say so when an image
  is fetched. A download that meets a full disk (`ENOSPC`) fails with "The disk is full: <file>
  needs <n> more on the disk, <f> is free. Free some space; Retry goes on from the <kept>
  already here", and its `.part` stays (D48).

**WP6's calls (2026-11-04, the build's own)**
- **D90 — Get it sends what the card shows, and the header names the class.** `model_get` gets
  the card's upload and file, `measure` as the shown fit's (the beside pick's `beside`, the whole
  card's pick's `whole_card`, D120), and `model_id` as the original's repo name in lower case
  (D28, D84); `target` only for a file chosen in the file list, whose class switch said it, and
  otherwise none, so the header decides (D83; after the card's header read it is cached and
  costs nothing). A file that fits neither measure is said against the whole card, its size
  against the card's ("Needs 232 GB on your graphics card, more than the 24.0 GB it has"), and
  Get it gets it with `whole_card`.
- **D91 — Cancel is the get's own job cancel. Ruled by the owner 2026-11-04 (ruling 5): Cancel
  on a joined get also cancels the get it joined, and stays so.** §6.1 says Cancel is `hf_set action=cancel` per
  file; the card sends `job_cancel` with the get's job instead, which since WP5's review round
  cancels the get and only the downloads it started that no other get waits for (D81). A cancel
  per file would fail the get with a cancelled download's sentence and could stop a projector
  another get shares. Its `.part`s are what a cancelled download keeps (D48): "What was
  downloaded is kept for the next Get it" is the card's line, and Get it again re-queues.
- **D92 — The time left is the download's own rate.** Once the get's bytes on the jobs feed have
  moved for 3 seconds, the rate is the bytes moved since the first frame over the time since it;
  before that, the card's estimate (D23). It shows only while the stage is `downloading`.
  Each file's line is its `hf_download` job's bytes (`hf:<id>`); a file whose job left the feed
  is done, with its last total.
  - *D92, revised in the review round.* A file whose transfer left the feed is done only when its
    last bytes reached its total, or its download row says `done`; short of it, it is `paused`
    when the get's stage is `paused` or its `waiting` names the file, `failed` when its row says
    so, else `waiting`. A row that says `done` with no transfer seen was on the disk before the
    get: "<file> · already on the disk", never "· 0 B". The rows' statuses are `model_get`'s
    answer's, then each status read's; a transfer that leaves the feed short asks for a read. The
    rows are keyed by file alone, so a frame moves a bar instead of rebuilding the row.
- **D93 — The end is read, never polled.** The card reads `model_get_status` once as the progress
  mounts and on each jobs frame that no longer lists the get's job (finished, or not started yet),
  one read at a time; `done` shows the result, `failed` the get's sentence with "Get it again",
  `canceled` the line of D91. A `model_get` answered by a row that already serves the file
  (`job_id` none, D87) is done at once.
  - *D93, revised in the review round.* A read wanted while one is out is kept and goes out when
    that one is back (a frame that drops the job mid-read was lost, and the progress hung after
    a Cancel); a `running` answer while the job isn't on the feed reads again 2 seconds later
    (the job not listed yet, or dropped before the read was answered), and stops once the job is
    on the feed. The reads, the timer and Cancel's continuation are the progress's own (a
    `Scope` made beside its values), so none runs on once a new search replaced it; a failed
    Cancel still toasts. The rules are `progress/track.rs`, unit-tested.
- **D94 — D9's answer is GB, saved as MiB after a confirm.** "How much memory does your graphics
  card have? · VRAM" takes GB (a card's "24 GB" is 24 GiB), and "Use this" asks "Save 24 GB as
  the graphics card's memory in Settings → GPU? lmgw then measures every model against it."
  before `settings_set_full` writes `vram.budget_mb` = GB × 1024; then the same search runs
  again, so the marks follow. The question shows when the shown fit's mark is `no_card`; Get it
  stays possible.
- **D95 — The card's disk check is the files', and asks once more.** Before Get it the card
  compares the download's bytes (the file and its companions, by the listing) with
  `disk_free_bytes`; short of room it says D125's line and the button turns into "Get it anyway".
  The engine image isn't counted, as in `model_get`'s own check (D89); `model_get`'s answer's
  `disk.sentence` shows under the progress when the server finds it short.
  - *D95, revised in the review round:* see D144; the line ends "Press Get it anyway to
    download it all the same", naming the button.
- **D96 — A file that doesn't fit keeps Get it, not as the primary button.** Confirm, then do:
  the card doesn't refuse, but Get it on a file whose shown mark is `does_not_fit` or
  `does_not_fit_by_size` is a plain button with "It doesn't fit your graphics card: it would be
  refused when it loads"; `NothingFits` adds "The file list shows every file of the upload with
  its fit" above "Choose the file myself".
- **D97 — "Show more" is the next window of one list; the hub's next page follows it.** "Show
  more (N not shown)" asks `model_find` again with that list's offset and the default count and
  the other list's count 0, and appends; once a list's window is whole and the answer had a
  `next_cursor`, "Look further on Hugging Face" asks with the cursor and starts both lists' next
  windows. Each costs the search and the walk again, as D142 says; the button waits while the
  hub's limit does.
  - *D97, revised in the review round.* Each list keeps its own hub page, its next page and how
    many of its page's entries it took: "Show more" asks for that page's next window (offset =
    taken on this page, as the server windows the page's own list), "Look further" moves only the
    list that asked and leaves the other on its page and cursor. A later page's card leads that
    page's entries in "Other models with this name" (the server leaves the card's original out of
    that page's `others`; `ModelCard` gains `likes` and `downloads` for its line), and an entry
    already listed, or the card the finder shows, isn't listed twice. The count is the pages'
    totals. The arithmetic is `model_finder/paging.rs`, unit-tested; the drive's fake has a page 2.
    While the hub's limit runs, the lists' entries are disabled too.
- **D98 — The Models page head has one primary button.** "Find a model" is the primary, a link
  to `/models/find`, before "Add from Hugging Face", which stays as a plain button (it was the
  primary). The find page is a `PageFrame` of its own (title "Find a model", a "Models" link in
  its head, no Models tabs), and `?q=` follows each search, so a reload or a link shows the same.
  The Overview's line sits under "No models configured yet" in the running table's empty state.
- **D99 — The card's smaller rules.** The hold's line shows from the live `vram` frame's
  `hold_active` or a fit note equal to `HOLD_LINE`, and the fold leaves the hold out of the notes;
  the machine check's line is `MachineInCard` as the labs use it. Marks are by size until the
  header's answer is in ("fits by size · checking…", the context line "checking…"), then that
  answer's recommended file replaces the card's (D123; D100 keeps the file a pasted address named); a marks answer for another upload than
  the card's is ignored. The whole card's pick has its line and "Use this one instead", which
  turns into the way back. A file chosen in the file list keeps its own fit and class, and
  `PickedFile` gains `size_bytes` and `quant` for the card's download line; the list's
  companions box isn't followed by Get it yet (Not yet). The fold lists the answer's other
  uploads, each opening the file list on it.
  - *D99, revised in the review round.* The pick-only file list offers only chat and aux (the
    classes `model_get` sets up) and shows no companions box (Get it fetches the projector and the
    drafter itself), so the fold no longer says so. A file chosen from a repo that is neither the
    card's upload nor one of its `other_uploads` goes to Get it with `model_id` none, so the server
    names it. `HOLD_LINE` is one constant, `lmgw_api_types::model_find::HOLD_LINE`, which
    `modelinfo::fit` re-exports. The licence shows as the hub names it (`apache-2.0`): there is no
    licence-name helper to prettify it with. Raw errors (a failed search, a header that couldn't
    be read) sit in a "What went wrong" fold under a plain sentence. The rate limit's notice says
    the Find button's live number ("wait 38 seconds" beside "resets in 38 s"), from
    `HubBudget.allowance` (new: the allowance sentence alone), which the button's tooltip also
    uses, so it says "With your Hugging Face token" when one is set. The progress bar is the
    accent blue.

**WP6's review round's calls (2026-11-04, the build's own)**
- **D143 — Every byte figure is said in binary units (revised by the owner's ruling of
  2026-11-04: "files are binary not decimal").** File sizes, downloads and their progress, disk
  space, graphics-card memory, transfer rates, body sizes, notes, MCP tool descriptions and the
  sentences in op answers all say `KiB`, `MiB`, `GiB` and `TiB` (`B` below 1 KiB), through one
  formatter, `lmgw_api_types::units::fmt_bytes` (with `fmt_bytes_of` for "1.2 of 3.4 GiB" and
  `fmt_bytes_so_far`). lmgw-core's `hf::fmt_bytes` and the dashboard's
  `fmt::fmt_bytes` are re-exports of it, so the server, the dashboard, the Chat and the client
  can't drift; there is no decimal GB, MB or kB and no figure in both units. A card sold as
  "24 GB" has 24 GiB (24 576 MiB). D9's question asks for GiB and its confirm says the GiB figure
  only ("Save 24.0 GiB as the graphics card's memory …"); the card then says "24.0 GiB your
  graphics card has". By size, the card's line is "Needs about 6.4 GiB of the 19.9 GiB free …":
  the files and the headroom, where it said "Its files take" beside a smaller download figure.
  The precision rule is D160.
- **D144 — Once a get stopped, the card leaves the disk to the server.** Before any get the card
  checks the whole download against the free space (D95). After a Cancel or a failure part of it
  is on the disk, which only `model_get` counts (its `disk.needs_bytes` less what is kept), so
  the card's own line and its "Get it anyway" go, and the server's `disk.sentence` shows under
  the progress when it is still short. Why not count it in `model_find`: it would read the
  downloads table and each `.part` for every search; a file already whole on the disk before
  any get still gets the card's question once (Not yet).
- **D145 — Get it keeps the file it got.** The card's shown file is frozen when Get it is
  pressed, while the header's marks may still be on their way ("checking…"), and stays through
  the progress, a failure and a cancel, so marks that land after it don't switch the card (and
  "Get it again") to another file. A file chosen in the file list unfreezes it and puts a stopped
  get back to "Get it".
- **D146 — Disposed values.** The progress's reads and Cancel (D93) and "Say hello" on the find
  page read nothing of theirs once they are gone: Say hello toasts a failed create in any case
  and opens the thread only while the page is there (`Scope::alive`). The rest of the finder's
  tasks run on the finder's own scope and read only its values.

**Owner rulings round 2026-11-04 (B)'s calls: binary byte units everywhere (2026-11-04, the build's own)**
- **D160 — One precision rule for every byte figure.** `lmgw_api_types::units::fmt_bytes`: whole
  bytes below 1 KiB ("812 B"); from there the largest unit whose rounded figure stays below 1024,
  one decimal below 100 of it and whole figures from 100 ("1.5 KiB", "24.0 GiB", "644 MiB",
  "1020 MiB"). The unit follows the rounded figure, so 1023.97 MiB is "1.0 GiB" and 99.96 KiB is
  "100 KiB", never "1024.0 MiB" or "100.0 KiB". It is the rule the dashboard's old `human_bytes`
  had for its decimal units, kept so a figure's length doesn't change with its unit. Progress
  (`fmt_bytes_of`) says both numbers in the total's unit and decimals, the part done rounded
  down and never the total while bytes are missing; bytes with no total (`fmt_bytes_so_far`) are
  rounded down too. lmgw shows no transfer rate yet; one would be the size per second.
- **D161 — The machine check says the card, the disk and the downloads in GiB.** Its own `gb()`
  (decimal GB, whole from 10) and `download()` (MB below a gigabyte) are gone: "Graphics card ·
  NVIDIA GeForce RTX 3090 · 22.5 GiB", "Room for models · 384 GiB free.", "1.2 of 3.4 GiB". The
  card line shows the total the driver reports (NVML's `memory_total`), which can be below the
  card's nominal size: a card sold as 24 GiB can report 22.5 GiB (23028 MiB). The
  ui-kit's download line lost its "under 1 MB" floor: a tiny total is said in KiB or bytes like
  any other size.
- **D162 — Podman's pull progress is said from its bytes.** The compat pull's blob line read
  Docker's `progress` bar text ("48.88MB/79.13MB", decimal); it now says `progressDetail`'s
  bytes through `fmt_bytes_of` ("Downloading 46.6 of 75.5 MiB"), and the bar text is no longer
  read.
- **D163 — Hand-written figures in prose are converted, not recomputed.** The image recipes'
  notes ("21 GiB on disk", "an 18 GiB DiT"), the decision plan's CPU note (2.22 GB → 2.07 GiB,
  podman stats being decimal) and the image plan's measured pipeline (6 221 MiB of weights from
  sd.cpp's log → 6.1 GiB) were converted from the figures they quoted; "1 to 16 MiB" for a
  header read is the measured range's order of size. Developer comments that record measurements
  in GB or MB keep them as measured: they are not shown to anyone.
- **D164 — Settings kept in MiB keep their unit and integer.** `vram.budget_mb`, the realtime
  limits, `max_body_mb` and the like are typed and stored as MiB and named so in their fields and
  in the messages that quote a setting ("realtime.max_message_mb (8192 bytes)", "max_body_mb = 32
  MiB"); a measured size beside them goes through the formatter. An agent's `memory_mb` is MiB
  (Podman's `--memory <n>m`) and is said through it ("2.0 GiB").

**WP7's calls (2026-11-04, the build's own)**
- **D100 — A pasted file stays the card's file while its header fits it.** The card by size
  already shows the file an address names when it fits (§3.2, WP4); the header's marks came after
  it and replaced it with their own recommended file (D99, D123), so the friend's card ended on
  the rules' pick, not the shared file. `ModelCard.named_file` (new) is that file when `file` is
  it; `shown::from_marks` keeps it while the header says it fits on either measure and passes to
  the rules' pick when it doesn't (the card's note, by size, already says which). The whole
  card's pick and "Use this one instead" are the other pick, as before.
- **D101 — The share text is built where the card's file is chosen.** `model_finder/share.rs`
  builds the text from the display name, the upload and the file the card *shows* (the beside
  pick, "Use this one instead", or a file chosen by hand), not from `card.share`, which is the
  file the answer started with. The text and the address's encoding are the server's
  (`find::card::share_url`: each path segment encoded, `main` not a commit), written again as a
  small pure function with a test of the encoding; `card.share` stays in the answer for API and
  MCP callers. The unit test fills every non-public field of a card (the maker, the account name,
  the original, the companions, the notes, the licence, the gateway's address, a port, a planned
  context) with a `*-demo` value and checks that none is in the text.
- **D102 — The button sits between "Choose the file myself" and "Get it".** §5's layout, a plain
  button. It copies through the browser's clipboard and answers with a toast ("Copied. Paste it
  in a message to your friend."); where the browser lets a page copy only over a secure
  connection (the dashboard opened from another computer over plain http) it says so in an error
  toast and copies nothing. There is no select-the-text fallback on this card yet.
- **D103 — Which repos a file can be sent from.** A repo the card knows is private (its upload,
  or an entry of `other_uploads` with `private`) shows D124's sentence in the card ("This model
  is private to your Hugging Face account, so there is no link to share.") and no button. A file
  chosen by hand in a repo the card doesn't list (a repo typed into the file list) shows
  neither: lmgw has no word on whether it is public, so it offers no link yet rather than risk
  naming a private repo.
- **D104 — The Find box takes the sentence as a person pastes it, and the server reads it.** No
  client-side extraction: the box sends what is typed, and `find::query` reads the address out of
  the text (§4.1). A single-line input drops the sentence's line break, so a pasted share text
  arrives as "…and pastehttps://huggingface.co/…": the parser already finds an address that
  starts inside a word, and a test on `find::query` and on the op (`model_find` with the share
  sentence, with and without its line break, a bare address, one without its scheme) pins it.
  The drive pastes through the input's `value`, which Chrome sanitises like a paste.
- **D105 — A file that doesn't fit the friend's card is said so, with the file that does.** The
  existing note (§3.2: "<file>, the file the address names, needs … and the card has …, so the
  card shows the file of the same upload that fits by its size") is the whole answer; an op test
  pins it on an 8 GiB card against the 9.5 GiB file. `named_file` is none then.
- **D106 — The paused line is one sentence.** The stage line of a paused get showed the server's
  `waiting` text whole ("Paused: <file>: <reason>; <file>: <reason>"), repeating the file names
  the rows below already carry. `words::paused_line` reads each reason once, in order, and says
  "Paused while the connection is metered." for the metered case; reasons that differ are
  joined with "; " ("Paused while the connection is metered; a download waits to start."). It
  parses the text the server writes, which a later `waiting` shape change would have to follow.
- **D107 — The README line sits in the quick start's second step.** One sentence after "add a
  provider under Upstreams": Models → Find a model, and that a link a friend sent (their "Send to
  a friend") is pasted into that box.

**WP7's review round's calls (2026-11-04, the build's own)**
- *D100, revised in the review round.* The pasted file stays the card's unless the header
  positively says it fits nowhere (`does_not_fit` on both measures). An entry without a fit (a
  header that couldn't be read), a mark by size or a file the marks don't list keeps it.
- *D101, revised in the review round:* see D109; the text and the address are no longer written
  twice.
- *D102, revised in the review round.* The clipboard is the kit's (`lmgw_ui_kit::clipboard`:
  `clipboard()`, `write_text`, the refusal's words), used by Send to a friend, the machine check's
  "Copy the commands" and `copy_secret`, which now says a page without a clipboard (not a secure
  context) too. A refused copy's toast says the browser's reason ("Not copied: …").
- *D106, revised in the review round:* see D108; the line no longer parses the server's text.
- **D108 — Why a get waits is typed on the wire.** `ModelGetDetail.waiting` and
  `ModelGetStatus.waiting` are `Vec<GetWaiting>`: each waiting file with a `WaitReason`
  (`metered`, `not_started`, `other`) and the download row's own `note`. The server decides the
  reason (`jobs::model_get::wait_reason`: the note equal to `hf_download::PAUSED_METERED_NOTE`
  is `metered`, none is `not_started`, any other is `other`). The finder's stage line says one
  sentence per distinct reason and no file names: "Paused while the connection is metered. It
  goes on once the connection isn't, or from the Downloads page.", "Paused until a download
  starts.", and an `other` note said whole ("Paused: <note>."). The metered note itself, which
  names the Downloads page's Resume, isn't shown in the finder. A full disk isn't a waiting
  reason: the download fails, and so does the get, with WP5's sentence (`failed_download`).
- **D109 — One share address and text, and a private original's name stays home. Ruled by the
  owner 2026-11-04 (ruling 8): stays as built.**
  `lmgw_api_types::model_find::{share_url, share_text, share_name, display_name}` are the only
  ones; `find::card` and the dashboard's `share.rs` both call them. Every byte of a segment but
  RFC 3986's unreserved ones is percent-encoded (the url crate's segment encoder left `[ ] ^ |`
  raw, and the Find box's reader stops at brackets and quotes). `ModelCard.origin_private` is
  new: when the original is private to the account, a share text from a public upload of it names
  the model by the upload's name (less `-GGUF`), not the original's.
- **D147 — The other line is said by its file's measure, and leads back to a pasted file.** The
  line under the card is labelled from the other file's `fit.measure` ("Beside what runs now" or
  "Once …"), not from which of the two is chosen. Under a pasted file that is neither pick, it
  offers the pick of the measure the pasted file doesn't lead with (the beside one under a file
  that leads on the whole card), and "Use this one instead" there leads back to the pasted file,
  also when both picks are one file.

**WP8's calls (2026-11-04, the build's own)**
- **D110 — The finder's end reads the model catalog again.** The live round's first "Say hello"
  opened a Chat whose model picker said "qwen3-0.6b · not served" before and after the model's
  real reply: the session's catalog (`/v1/models`) is fetched when the session opens, before
  the get made its row, and nothing read it again. `progress::Ready` now refreshes the catalog
  when it shows, in any host of the finder, so the Chat it leads to knows the model; a reload
  had shown it served all along.
- **D111 — A refused copy is said in one sentence.** Chrome's refusal ends in its own full stop
  ("… Document is not focused."), and the share button's toast added one ("…focused.."). The
  kit's `clipboard::refusal` drops the reason's closing stops, as every caller puts it inside a
  sentence of its own, and `copy_secret`'s toast is "Not copied: <reason>." like the others.
- **D112 — The 8 GB half of §4.4 ran as a declared budget on a real card.** `model_find` has no
  card argument, so the set ran twice through the op: on the 24 GiB test GPU as it is, and with D9's
  own setting (`vram.budget_mb` 8192, saved by `settings_set_full` as the finder's question
  saves it) and back to 0. A declared budget is the whole card (D5); the beside measure then
  also takes the outside use of the card (0.7 GiB) off the 8 GiB. The comparison is by the card
  the walk gives (its original, and its upload where §4.4 names one), the regression test's
  own measure; the files are the header fit's, read separately for the files §4.4 flagged.
- **D113 — "The app quit" is the quit signal, not a kill.** The headless instance got SIGTERM by its
  PID, 40 % into a download: `server::quit_signal`, then the teardown the tray app's Quit runs
  too (`runtime::lifecycle::shutdown`). A crash (SIGKILL) keeps the same `.part` and boot path and was played
  on the mock CDN by WP1 and WP5; it wasn't repeated live.
- **D114 — Two live findings stay as found.** "qwen3 1.7b" answering another kind, and a split
  set's input embeddings charged to the card (Not yet), each need a rule change (D15 and D126's
  ranking; D3's footprint, which admission shares) that is the owner's or a work package's,
  not a live round's small fix. Both are recorded with their numbers. *Ruled 2026-11-04:* the
  owner settled both (rulings 3 and 6); the first is built (D15, D148), the second is fixed (D175).

**The owner's rulings of 2026-11-04: the calls (the build's own)**
- **D148 — The kind leads even a looser match.** Ruling 3 says "among originals that match every
  word"; every original matches every query word, exactly or loosely (D13: a repo with a word
  missing is only a "Did you mean" line), so the kind is the first key of D15's order, before
  the match. A chat original whose name only starts a query word ("1.7bx" for "1.7b") comes
  before a speech model that matches it exactly. The other place, after the match, would leave
  "qwen3 1.7b" as ruled (both match all three words exactly) and differ only when the only exact
  matches are other kinds; then the answer would be another kind where a chat original matched
  loosely. "Other models with this name" follows the same order, so the other kinds sit at its
  end. The 28 queries of §4.4 can't change under it: none of them answered another kind, so each
  top already was of a kind Find a model sets up and stays first, and the walk set (the top's
  author, kind and match) is the same set.
- **D149 — Queries outside §4.4 get their own list, recorded beside it.**
  `tests/it/hf_find/regression.rs` has `LATER` beside `QUERIES`: a query a live finding changed a
  rule for, with its card on both measures ("qwen3 1.7b" first). `scripts/hf-find-record.sh
  --add` keeps the last recording's answers and reads only what they lack, so adding a query
  doesn't refresh (and possibly move) the 28 recorded cards; a whole recording stays the way to
  refresh them, and the only way when `HubModel` gains a field. The index's date then says both
  ("2026-11-04, reads added …" when the days differ).
- *Built (2026-11-04):* `Kind::sets_up` and D15's key in `find/rank.rs`; `find.rs` answers
  another kind only when the top is one. *Tests:* `find::rank`'s
  `a_kind_find_a_model_sets_up_comes_before_another_kind` (an exact speech model with more
  likes behind the chat original, a loose embedder before both other kinds, the partial line and
  the walk set unchanged, only other kinds still another kind); `hf_find::regression`'s
  `a_kind_find_a_model_sets_up_leads_another_kind` over the recording of "qwen3 1.7b" (the card on
  both measures, the other kinds last, the speech model under the others), and the 28 queries
  unchanged.


## 1. What exists today

Lines at `main` 68ca8038, rechecked at 94c735b2.

**Hugging Face.**
- `hf_repo` lists a repo's files from the hub's tree API, every page (`hf/listing.rs:58`). It keeps
  the files the target can load, collapses a split GGUF into one entry with its parts' sizes
  summed (`ops/huggingface.rs:207`), and names each file's role from its name: `weights`, `mmproj`
  or `drafter` (`classify_repo_file`, `ops/huggingface.rs:27`), plus a quant label read from the
  name (`quant_label`, `:164`). It reads nothing inside the files, and `HfFile` (`hf.rs:396`) keeps
  only the path, the size and the type, not the listing's `lfs` hash (WP1 adds it, D4). The route is
  `GET /api/hf/repo` (`web/api.rs:85`) and the MCP tool is `lmgw__hf_repo`
  (`mcp/selfadmin/catalog/reads.rs:369`).
- `hf_add` downloads one weights file and, by default, its companions from the same directory: the
  projector, and for chat also the drafter (`ops/huggingface.rs:285`). It is refused before the
  hub is asked when the class has no models directory (`hf.rs:121`), and a fresh install has one
  per class since ab5fd404. It also fetches the engine image the file runs in (machine check §5).
- The downloads are `hf_download` jobs with byte progress (`jobs/hf_download.rs:113`), started
  and finished times (`jobs/mod.rs:180`), and a durable row in `hf_models`. `hf_downloads`
  answers them (`ops/huggingface.rs:497`), and the SSE `jobs` frame on `/api/events` carries them
  live (`web/api.rs:496`).
- The hub's refusals have sentences: gated (`hf.rs:133`, `is_gated_response` `:185`), a rejected
  token (`:144`), a rate limit with the window's reset read from the `RateLimit` header
  (`ratelimit_reset`, `:432`; `ListFailure::RateLimited`, `hf/listing.rs:21`). A listing page
  times out after 30 s (`hf/listing.rs:140`). There is no search: everything starts from an
  `owner/repo` the person already knows.
- `get_with_commit` (`hf/resolve.rs:51`) walks the hub's hops itself and hands the CDN's hop to
  reqwest. It takes no request headers of its own (so no Range) and returns the final response
  and the commit only, so the hub hop's `x-linked-etag` and `x-linked-size` are dropped. WP1
  changed both (D40).

**Bug H1: `hf_add` fetches every projector in the directory.** Its comment says "a repo with
several quants ships exactly one projector/drafter shared by all of them"
(`ops/huggingface.rs:384`), and the loop after it adds every same-directory file whose role is
`mmproj` (`:396–410`). `unsloth/Qwen3.6-27B-GGUF` ships `mmproj-BF16.gguf` (0.93 GB),
`mmproj-F16.gguf` (0.93 GB) and `mmproj-F32.gguf` (1.84 GB), so a default `hf_add` there fetches
3.70 GB of projectors where one is used. The disk plan then takes whichever projector its
directory scan lists first (`modelinfo.rs:1388–1404`, `"mmproj" if mmproj.is_none()`), not a
chosen one. D121 is the one rule, and WP1 fixed the code with it (both places, the aux plan's
first-found pick and the capabilities note's first-sorted name).

**Bug H2: a download dies with the process.** Boot fails every queued and running job
(`fail_orphaned_jobs`, `store/jobs_table.rs:179`, called at `state.rs:591`), and a download
starts its `.part` from byte 0 (`File::create`, `jobs/hf_download.rs:179`; the cancel path's
comment says "the transfer has no resume"). A 17 GiB download cut by quitting the app starts over at the next launch. *(Corrected in WP1, D45: not by hand. After the job rows are
closed, `spawn_background_tasks`, `server.rs:883`, queued every `hf_models` row left `queued` or
`downloading` again, and the transfer began at byte 0.)* D122 and WP1 and WP5 fix it; WP1's half
is built.

**Finding F1: a split GGUF's weights are under-counted, fixed separately.** `GgufFiles::read`
takes the weights' bytes from the first part's file alone (`vram/plan.rs:223`, `main_bytes:
file_size(&main)`), and `plan_fit::chat` takes them from the first part's header
(`plan_fit.rs:111`, `s.file_size`). A row on `…-00001-of-00002.gguf` is charged for that part
only. For unsloth's Qwen3.6-27B BF16 that leaves out 3.8 GB of its 53.8 GB. It is a bug of today's
code, and it is fixed in a separate commit on main, outside this design. WP2
depends on that fix being on main and doesn't contain it.

**The planner.**
- `local_model_plan` plans only a file on disk (`modelinfo.rs:1338`). It reads the header
  (`gguf::summarize`, `gguf.rs:1987`, over `read_header`, `:497`), finds the projector and the
  drafter in the same directory on disk and reads their headers, and returns every parameter with
  its reason. Its route `GET /api/local-model-plan` (`web/api.rs:76`) reads `InspectQuery`, the
  query `model_inspect` shares, with `path` required (`web/api.rs:399–403`).
- The fit checks read the disk themselves after the plan: `plan_fit::chat` stats the companions
  (`plan_fit.rs:86`) and reads the drafter's header (`:101`), and the aux fit stats the projector
  (`aux_plan.rs:383`). So "one plan input from either source" first needs those reads moved into
  the input (WP2).
- The header reader is generic over `Read` with the file's size passed in (`Reader<R: Read>`,
  `gguf.rs:758`). So it can read a stream, given the total length. It bounds every declared length
  by that size, and computes each tensor's bytes from the data offsets and the file's end
  (`gguf/tensors.rs`).
- What the footprint needs from the header is in `ModelSummary` (`gguf.rs:1604`): architecture,
  trained context, layers, heads and KV heads (per layer where they differ), key and value widths,
  the sliding window and its pattern, the recurrent state, the vocabulary size, the output head's
  bytes and the input embeddings kept in host memory. It also needs the file's bytes and the
  companions' bytes (`GgufFiles`, `vram/plan.rs:207`).
- A plan that doesn't fit the card warns with the largest context that fits and keeps the trained
  context (`plan_fit.rs:60` and `:131`; owner finding 3). The card it measures against is
  admission's: the budget or the devices' total, plus the headroom, and whether admission is on
  (`Card`, `plan_fit.rs:26`; `group_capacity`, `vram/beside.rs:53`). An aux plan that ties the
  ubatch to the context keeps the tie at a smaller context in its search (`aux_plan.rs:394–404`).

**The ledger.** Outside use is the devices' used bytes less lmgw's measured share
(`vram/ledger.rs:507`, `outside_of`, `vram/beside.rs:156`). Each resident row has its charge
(`ResidentView.estimated_bytes`, `vram/view.rs:26`). `crowded_beside` (`vram/beside.rs:79`)
already answers "is there room beside these kept rows" for the realtime voice's warm claims
(`realtime/warm/kept.rs:69`).

**The UI.** The Models page has "Add from Hugging Face" (`pages/models.rs:1156`). It opens
`HfWizard` (`pages/wizard.rs:111`): a repo field, a class choice, the companions box, and the file
list sorted by role, then quant width, then size, with nothing picked (`wizard.rs:173`: "Nothing is
picked for the user"). Nothing told it which file fits. When the downloads are done, the page plans
and creates the row itself, and never starts it.

**The machine check.** The verdict is `MachineSummary` (`lmgw-api-types/src/machine.rs:468`) on
`GatewayStatus.machine` and in the `machine` frame. `MachineInCard` shows its line inside a card
while the verdict isn't `ready` (`widgets/machine_check/surfaces.rs:126`). The Chat gets the check
through a slot its host fills, `ChatMachine` (`pages/chat_machine.rs:23`), so the Chat crate never
reads the admin plane. The engine pre-fetch reads whether the connection is metered through
`nmcli` (`metered`, `machine/prefetch.rs:115`; machine check D99).

**Links.** lmgw registers no URL scheme of its own. `lmgw-pair:` is the scheme of the clients it
pairs with, and lmgw only hands such a link on to the desktop (`src-tauri/src/pairing.rs:28`).

## 2. Reading a file's plan before downloading it

### 2.1 The read

1. One `GET` of `https://huggingface.co/<repo>/resolve/main/<file>` through `get_with_commit`,
   which WP1 changed in two ways (D40): it takes request headers for the CDN's hop (here
   `Range: bytes=0-`; the hub's own hops get none, and no `HEAD` comes before), and it returns the
   hub hop's `x-linked-etag`, `x-linked-size` and `x-repo-commit` beside the response. The hub
   answers `302` to its CDN with those headers. The CDN answers `206` with
   `Content-Range: bytes 0-…/<total>` and its own `ETag` (the xet hash). Measured §2.2.
2. The body is read as a stream into the header reader, with `<total>` as the file's size.
3. At the first byte of tensor data the stream is dropped. The reader then has the metadata and
   every descriptor, and `tensor_bytes` follow from the offsets and `<total>`, as on disk.
4. The summary is cached by the listing's `lfs.oid`, which equals `x-linked-etag` (D4).

A card reads the weights file's header and the projector's (tens of KB), and a drafter's when the
upload ships one. For a split file, only part 1 is read (D3). The other files of the same original
are estimated from that header (§3.3, D123). The same header change carries the resumed
download's `Range: bytes=<reached>-` (D122).

`gguf.rs` gains `read_header_from(reader, total)`, and `read_header(path)` becomes that over a
`BufReader<File>`. The planner keeps one reader and every guard it has. A short read (the CDN cut
the stream) is `UnexpectedEof`, as an interrupted download is today. The summary gains the
parameter count (each descriptor's element count, summed) and the repeating layers' bits per
weight (D7), both from the descriptors.

*Built in WP1:* `hf::remote_header` (`hf/remote.rs`) is steps 1 to 4. The body goes chunk by
chunk across a channel to the reader on the blocking pool, through a 64 KiB `BufReader` as a
file on disk is read (without it, the reader's string-by-string skips cost 10 s for the 27B
header in a debug build, against 0.8 s on the wire), and the pump stops asking for chunks as
soon as the reader returns. The stall bound is D44, the cache D43, the new facts D49, the
memory bound D129.

### 2.2 Measured (2026-11-03, public files, no token; a spike script, not committed)

| File | Total | Header + descriptors | Of it, tokenizer | Tensors | One request |
|---|---|---|---|---|---|
| unsloth/Qwen3.6-27B-GGUF `Qwen3.6-27B-Q4_K_M.gguf` | 16 817 244 384 | 10 993 857 | 10.93 MB (merges 5.09, tokens 4.85, types 0.99) | 851, 50 KB of descriptors | 0.74 s, first byte at 0.49 s |
| unsloth/gemma-4-12b-it-GGUF `gemma-4-12b-it-Q4_K_M.gguf` | 7 121 861 440 | 15 824 191 | merges 9.25 MB, tokens 4.41 MB | 667 | 0.86 s, first byte at 0.57 s |
| unsloth/Qwen3.6-27B-GGUF `mmproj-F16.gguf` | 927 607 360 | 20 094 | none | 334 | in the first 1 MB |
| …`BF16/Qwen3.6-27B-BF16-00001-of-00002.gguf` | 50 004 497 824 | 10 977 686 | as Q4_K_M | 576 of 851 (`split.count` 2) | as above |
| …`BF16/Qwen3.6-27B-BF16-00002-of-00002.gguf` | 3 803 783 936 | 16 111 | none, 3 keys | 275 | in the first 1 MB |
| bartowski/alamios_Mistral-Small-3.1-DRAFT-0.5B-GGUF `…-Q4_K_M.gguf` (a drafter) | 460 616 320 | 7 878 783 (1.7 %) | merges 5.01 MB, tokens 2.32 MB, types 0.52 MB | 291 | 4 growing chunks, 3.2 s |

- **The minimum is the header itself.** The tokenizer's string arrays are 99 % of it and must be
  walked string by string to find the descriptors, so no read can skip them. A model with a large
  vocabulary costs 11 to 16 MB. The descriptors are about 60 bytes a tensor.
- **A drafter carries the full tokenizer too** (review M3, measured above): it has to share the
  big model's vocabulary to draft for it. A card with a drafter reads about 8 MB more.
- **One open-ended request beats chunks.** Growing 1 MB chunks took 4 to 5 requests, fetched 15 to
  32 MB and took 3.3 to 4.4 s for the same two files.
- **The CDN.** `huggingface.co` answers `302` to `us.aws.cdn.hf.co/xet-bridge-us/…` with a signed,
  expiring URL. Files on the xet store are served by the bridge with `accept-ranges: bytes`. A
  `206` carries `Content-Range` with the total and an `ETag` that is the xet hash, not the sha256.
  The sha256 is the hub's `x-linked-etag`, which is what lmgw's update check already compares
  (`hf.rs:474`; *corrected in WP1, D42:* read in code, the update check compares the final
  response's ETag, which is the CDN's). The hub's resolver limit is its own bucket:
  `"resolvers";q=3000;w=300`, separate from the API's `"api";q=500;w=300` (anonymous, read from
  `RateLimit-Policy`).
- **lmgw's own read, by hand (2026-11-04, WP1's `remote_header` uncached, no token, a debug
  build):** `mmproj-F16.gguf` 0.79 s, data start 20 096, 30.8 KB received, 460 730 096
  parameters; `Qwen3.6-27B-Q4_K_M.gguf` 0.87 s, data start 10 993 888, 11.16 MB received,
  26 895 998 464 parameters, 4.94 bits per weight over the repeating layers, `qwen35`, context
  262 144; the BF16 file's first part 0.82 s, 24 995 439 872 parameters in its own tensors (D49; since
  D131 that is `part_parameter_count`, and `parameter_count` is none for a part), 16.0 bits. Each hop's `x-linked-etag` equalled the listing's `lfs.oid`, and curl's read of the
  same 11.16 MB took 0.79 s.
- **A bulk read** of 200 MB from the same CDN ran at 66.7 MB/s on the test connection. The header reads ran at about
  44 MB/s after their first byte, so an estimate from them is low, which is the safe side (D23).
- **On a slow line the header read is what the person waits for.** 11 MB is 0.7 s on the test connection; at
  50 Mbit/s it is about 2 s, at 16 Mbit/s about 6 s, at 6 Mbit/s about 15 s, plus 8 MB more with a
  drafter. That is why the card comes first with its size marks and the header refines it (D123),
  and why the page doesn't read headers by itself on a metered connection.

### 2.3 What the planner takes from it

The plan has one input struct, filled from either source:

```text
PlanFiles {
    summary: ModelSummary,          // the weights' header (part 1 of a split file)
    weights_bytes: u64,             // every part, from disk or from the tree listing (D3, F1)
    companions: Vec<Companion>,     // projector (D121), drafter: path, role, bytes, summary
    source: Disk { dir, rel } | Hub { repo, file, oid },
}
```

`local_model_plan` builds it from disk. `plan_from_hub(repo, file)` builds it from the tree
listing (sizes, the companions in the same directory, by `classify_repo_file` and D121) and the
remote headers. Everything after that is the same code, `plan_fit` and the aux fit included, so a
plan from the hub and a plan from disk of the same file agree. That takes a first step: the fit
checks' own disk reads (`plan_fit.rs:86` and `:101`, `aux_plan.rs:383`) move into `PlanFiles`, so
nothing after the input touches the disk (WP2). §10 holds the two sources equal.

*Built in WP2:* `modelinfo/plan_files.rs` (`PlanFiles`, `Companion`, `Source`, `from_disk`) and
`plan_files/hub.rs` (`from_hub`). Beside the sketch it keeps each file's missing-part sentence,
the whole model's parameter count and layer width (D54), the hub reads' bytes and time (D58),
the class whose dir the file is in or goes to (`found_in`, D51), and why the fit is by size when
a split set can't load as it is (`size_only`, D54).
The plan names its companions by the paths a row takes, and a hub plan by where `hf_add` puts
them (D51); on the hub a companion is found by its name (D52).

## 3. What "fits" means, and which file is recommended

### 3.1 The measure

`Fit` is one function over a `PlanFiles`, a capacity and the planned shape (slots, batch sizes,
cache types, the projector's ubatch floor, the drafter), all from the plan. It is taken against
the two capacities of D5 and D120:

```text
Fit {
    verdict: Fits | FitsShorter | DoesNotFit | BySizeOnly { fits: bool } | NoCard,
    ctx: Option<u64>,               // the largest that fits, ≤ trained (D6)
    trained_ctx: Option<u64>,
    footprint: Footprint,           // at ctx: weights, KV cache, compute buffers
    capacity: u64, headroom: u64,   // Card (D5), less the beside terms for that measure
    measure: WholeCard | Beside { outside: u64, residents: u64 },
    bits_per_weight: Option<f32>,   // the repeating layers' (D7)
    notes: Vec<String>,             // the hold, admission off, why a term is missing
}
```

When admission is off, "fits" still means fits: nothing refuses the start then, and the note says
so as `over_card`'s warning does today. Without a device reading there is no outside use to read,
so the beside measure is the whole card less lmgw's residents, and the note says so.

*Built in WP2* as `fit::fit` over `plan_fit::Sizer` (D136), with the wire form `FileFit` (D137);
`verdict` is its `mark`, and the beside terms are `CardMeasure` and `BesideTerms` (D55).

### 3.2 The recommended file

Among the repo's weights files (a split file being one), for each measure, in this order (D7,
with its 8.5-bit cap):
1. the largest file that fits at the trained context with 4 to 8.5 bits per weight;
2. else the largest file that fits at least min(trained, 32 768) tokens with 4 to 8.5 bits per
   weight;
3. else none, and the card says the model is too big for this card at a quality worth having, with
   the file list one click away.

The "Why this file" line reads, for example: "The largest file that fits beside what's on your
24 GB now with the whole conversation length the model can take." For rule 2 it reads: "The model
can remember 262 144 tokens. On your card, the largest file that still remembers 32 768 is this
one." With D7's refinement, the fold also names the file that remembers the whole trained context
when one does. No other rule decides, and the file list shows every file's mark, so the choice can
be checked by eye.

### 3.3 Marks on a file list without reading every header

A repo of 26 files would cost 26 header reads, about 290 MB for a Qwen-sized vocabulary. So
(WP3):
- the recommended candidate's header is read, and every file's fit is first estimated from it:
  the file's own bytes charged in full (not less its input embeddings, which differ by quant), and
  the KV cache and compute buffers from the shared architecture. That estimate is never smaller
  than the real one, so "fits" from it is true.
- A file whose estimate doesn't fit, but would with the reference's host-kept input embeddings
  taken off, is read exactly. Only the files near the edge are in that band.
- The file the person clicks is read exactly before "Download & create" (or, in the pick-only
  mode, "Use this file") is enabled.
- `RepoFiles` reports the header bytes it read (D123).

| Mark | When |
|---|---|
| `fits · 262 144 tokens` | fits at the trained context |
| `fits · 65 536 of 262 144 tokens` | fits a shorter conversation |
| `fits once other programs close` | fits the whole card, not beside what runs now (D120) |
| `doesn't fit` | not even at one token |
| `fits by size · not checked` | header not read yet or unreadable, size alone fits (D8, D123) |
| `recommended` | the one file of §3.2, beside its fit |
| `checking…` | the exact read is running |

Each mark's tooltip holds the footprint's terms in GB: "uses 21.4 of 24 GB: weights 16.0, the
conversation 3.9, working memory 1.5", and for the beside measure what else is on the card:
"other programs 6.2, lmgw's voice models 2.1".

### 3.4 Around the card

- *Something else on the card now* (D120): the card leads with the beside pick and names what is
  there: "Other programs use 6.2 GB of your graphics card right now, and lmgw's voice models
  2.1 GB. This file fits beside them."
- *The GPU hold* (D10): the paused line, with where to resume.
- *No capacity* (D9): no marks, the machine check's line, and the inline question for the card's
  memory.
- *Several cards pooled:* the pooled total, as admission sees it.
- *A full disk* (D125): the line under the download size.

### 3.5 Context in words

One label at every depth (D39): **How long a conversation it can remember** · context, 131 072
tokens. When the card fits less than the model can take, "· the model can take 262 144" follows.
On the card the number is what the row gets (D26). In the file list it is the mark's number.

## 4. Search by name

### 4.1 The query

The Find box takes, in this order:
1. a Hugging Face address: `https://huggingface.co/<repo>` or `https://hf.co/<repo>`,
   `…/blob/<rev>/<file>`, `…/resolve/<rev>/<file>`, `…/tree/<rev>/<dir>`, also inside pasted text
   such as §7's share sentence, and wherever it starts in a word (`[x](https://…)`,
   `url:https://…`). The repo and the file go straight to §4.3, with that file as the pick when
   it fits. The hub's model search page (`/models?search=<q>`) is its `search` text, a name.
   An account's page (`/<name>`), a dataset (`/datasets/…`), a Space (`/spaces/…`) and any other
   page that names no repo are answered locally, each with its own sentence (`not_a_model`);
2. any other address (`scheme://…` on another host): answered locally, never sent (D124);
3. `owner/repo`, validated as `validate_repo` does (`hf.rs:221`): straight to §4.3;
4. anything else: a name, to §4.2.

### 4.2 Finding the original

Two hub calls (D12), each `sort=downloads&direction=-1` with `expand[]` = author, downloads,
likes, gated, private, baseModels, pipeline_tag, tags and safetensors (the parameter total for
D15's own weights and D17's pass-over; it rides the same call, about 6 % more bytes, 47.8 KB to
50.6 KB for "mistral small", measured):
- `/api/models?search=<q>`;
- `/api/models?search=<q>&filter=gguf`.

From the union: match (D13), originals (D14), rank (D15). The hub's search already copes with
spacing and case: "qwen 3.6", "qwen3.6" and "Qwen3.6" answered the same, and "llama 3 8b" found
the Llama 3 8B repos. "gemma four" found only repos with "four" in the name (measured §4.4).

The card's kind follows the top original's pipeline (D126), and a kind Find a model sets up
ranks first (D15, ruled 2026-11-04). The header confirms it before the
download (D29).

### 4.3 Finding the file

For the top author's originals in turn (D17):
1. passed over with no call when the listing's parameter total at 4 bits per weight plus the
   headroom is larger than the card;
2. `/api/models?filter=base_model:quantized:<origin>&filter=gguf&sort=likes`: its uploads. The
   maker's own GGUF repo (D14) comes first;
3. plain uploads (D16), the maker's first. A gated one goes behind one that isn't (D20);
4. the upload's tree (`list_repo_files`, as `hf_repo` does), its weights files with their sizes
   and hashes. If the smallest file of at least 4 bits per weight plus the headroom is larger than
   the card, the next original is tried;
5. the card, with size marks (D123). The page then asks `hf_repo` with `fit=true`, which reads
   the candidate's header and its projector's: when the candidate doesn't fit after all, the next
   file down is estimated from that header (§3.3), then the next upload, then the next original
   of the same author; past the last one the answer is `NothingFits`.

### 4.4 Measured (2026-11-03, a spike script not committed, the revised rules, no names)

The review's three failing queries, under the first draft's D15 (the tag first) and under the
revised rules, read live the same day (a spike script for the first draft's):

| Query | First draft's top original | Revised rules' card |
|---|---|---|
| mistral small | `alamios/Mistral-Small-3.1-DRAFT-0.5B` (28 likes, tagged, a draft model) | `mistralai/Mistral-Small-3.1-24B-Instruct-2503` (1409 likes, not tagged) |
| deepseek v4 flash | `Jackrong/Qwen3.5-9B-DeepSeek-V4-Flash-GGUF` (344 likes, a Qwen finetune) | `deepseek-ai/DeepSeek-V4-Flash-0731` (4095 likes, not tagged): `NothingFits` on 24 GB |
| gemma 4 | Google's 31B-it and 26B-A4B-it, then `HauhauCS/Gemma-4-E4B-Uncensored-…` above Google's own E4B-it (not tagged) | `google/gemma-4-31B-it`, and on 8 GB Google's `gemma-4-E4B-it` |

The regression set: 28 queries, the review's failing ones, everyday names someone says out loud,
and four embedders, each against a 24 GiB card and an 8 GB one (a gamer's), with
1 GiB headroom (the setting's default, `settings_classes.rs:106`). The spike reads no header: its
file pick is by size, at 4 to 8.5 bits per weight from the listing's parameter total (whole-file
bits, not D7's repeating layers), so a file near the card's edge is the header fit's to decide.

| Query | Card on 24 GB (upload, file by size) | Card on 8 GB | Right card? | Calls, KiB, s |
|---|---|---|---|---|
| qwen 3.6 | Qwen3.6-35B-A3B (unsloth, UD-Q4_K_XL 22.4 GB) | NothingFits (smallest of 4 bits 18.0 GB) | yes | 5, 147, 0.9 |
| gemma 4 | gemma-4-31B-it (unsloth, UD-Q5_K_XL 21.9 GB) | gemma-4-E4B-it (not tagged, pipeline `any-to-any`) | yes | 7, 162, 1.3 |
| gemma four | NoRunnableFile: `tomaszki/nous-gemma-four` matches exactly and has no GGUF | the same | yes (M5); D18: no retry, the answer says numbers are written as digits | 4, 5, 0.8 |
| llama 3 8b | Meta-Llama-3-8B-Instruct (the base has more likes; within meta-llama the tag decides) | the same, Q6_K | yes | 5, 136, 1.0 |
| deepseek v4 flash | DeepSeek-V4-Flash-0731: NothingFits, 7 originals of 291 to 763 B parameters passed over with no call | NothingFits | yes | 3, 122, 0.6 |
| mistral small | Mistral-Small-3.1-24B-Instruct-2503 (bartowski, Q6_K_L 19.7 GB) | NothingFits | yes | 5, 144, 0.9 |
| qwen3 embedding | Qwen3-Embedding-0.6B, from the maker's own GGUF (Q8_0) | the same | yes | 5, 120, 0.9 |
| hermes | NousResearch/Hermes-3-Llama-3.1-8B (a GGUF-only finetune with 721 likes is behind it now) | the same, Q6_K | yes | 5, 128, 1.0 |
| llama | Llama-3.1-8B-Instruct (bartowski, Q8_0) | the same, Q6_K_L | yes | 5, 134, 0.9 |
| mistral | Mistral-7B-Instruct-v0.3 (the base v0.1 has the most likes; within mistralai the finetune of its own base leads) | the same, Q6_K | yes, an old model: likes favour old repos | 5, 137, 0.9 |
| phi 4 | microsoft/phi-4, from the maker's own GGUF (Q8_0 15.6 GB) | phi-4, but `Q3_K` at 4.02 whole-file bits | yes; the 8 GB file is wrong, D7's repeating-layer count drops it | 5, 129, 0.9 |
| gpt oss | gpt-oss-20b (120b passed over with no call) | NothingFits | yes | 5, 134, 0.9 |
| deepseek | DeepSeek-R1-Distill-Qwen-32B (5 originals of 684 B parameters passed over with no call) | R1-Distill-Qwen-1.5B | yes | 7, 150, 1.3 |
| qwen | Qwen3.8-27B (24.2 GB by size, at the card's edge) | Qwen2.5-7B-Instruct | yes | 7, 158, 1.2 |
| gemma | gemma-4-31B-it | gemma-4-E4B-it | yes | 3, 124, 0.7 |
| nomic embed | nomic-embed-text-v1.5, from the maker's own GGUF | the same | yes | 5, 114, 0.9 |
| bge m3 | BAAI/bge-m3 (`pytorch` weights only; KimChen's GGUF, its f32 file: no listing count, so no cap) | the same | yes; the file is the header count's to cap | 5, 113, 0.9 |
| embeddinggemma | google/embeddinggemma-2 (unsloth's BF16 file, which the listing's count puts at 6.0 bits) | the same | yes; the file is the header count's to cap | 5, 117, 0.9 |
| llama 3.2 3b | Llama-3.2-3B-Instruct (bartowski, Q8_0) | the same | yes | 5, 144, 0.9 |
| granite | granite-docling-258M, a document converter and IBM's most-liked repo | the same | **no** | 5, 116, 0.9 |
| smollm | SmolLM3-3B (ggml-org, Q8_0) | the same | yes | 5, 116, 0.9 |
| olmo | Olmo-3-32B-Think (the base OLMo-7B has the most likes) | Olmo-3-7B-Instruct | yes | 7, 122, 1.2 |
| glm | GLM-4.7-Flash (5 originals passed over with no call; GLM-OCR left out as another kind, D126) | GLM-4.1V-9B-Thinking | yes | 7, 134, 1.2 |
| nemotron | NVIDIA-Nemotron-3-Nano-30B-A3B-BF16 (24.5 GB by size, at the edge) | NVIDIA-Nemotron-Nano-9B-v2 | yes | 7, 162, 1.3 |
| gemma 3 | gemma-3-27b-it (unsloth, UD-Q6_K_XL 23.7 GB) | gemma-3-4b-it | yes | 7, 179, 1.3 |
| qwen 2.5 coder | Qwen2.5-Coder-32B-Instruct, from the maker's own GGUF | Qwen2.5-Coder-7B-Instruct | yes | 7, 166, 1.3 |
| ministral | Ministral-3-14B-Reasoning-2512, from the maker's own GGUF (the untagged 8B-Instruct-2410 has the most likes) | Ministral-3-3B-Reasoning-2512 | yes | 7, 136, 1.2 |
| kimi | Kimi-VL-A3B-Thinking-2506 (7 originals of 1 to 2.8 T parameters passed over with no call) | NothingFits | yes | 8, 113, 1.3 |

**27 of 28 queries pick the right card** on both cards. The miss is "granite": IBM's most-liked
repo is a 258M document converter, tagged `conversational` and with the chat pipeline
`image-text-to-text`, so no generic signal the hub gives tells it from IBM's chat models. Two
picks of a file are wrong in the spike and right under D7 (phi 4 on 8 GB, bge m3), one
more depends on it (embeddinggemma), and four sit at the card's edge by size and are the header
fit's to settle (qwen, glm, nemotron, qwen 2.5 coder on 24 GB).

What the runs showed, and what the rules took from it:
- **The tag can't rank across authors** (B1, above). Likes before the tag, and weights of its own
  before GGUF-only, put the maker's model first in every query but one.
- **Within one author, the tag or a finetune of the author's own base tells chat from base**:
  "llama 3 8b" (the base has 6779 likes, the Instruct 5701), "mistral" (v0.1 base first by likes),
  "olmo", "ministral".
- **Downloads are a poor signal for "the model".** Searching "qwen3.6" in GGUF repos, the top
  result by downloads was an abliterated upload (2.8 M downloads, 70 likes). The original-first
  rule leaves it out, and the likes rank puts Qwen's own repo first.
- **The maker's own GGUF must count as an upload of its original** (D14). Folded in, it is the
  file of "qwen3 embedding", "nomic embed", "phi 4", "qwen 2.5 coder" and "ministral".
- **The walk needs the kind and the match** (D126): "glm" and "gemma 4" above.
- **A count from the listing saves calls**: with D17's pass-over at 4 bits, "deepseek v4 flash"
  went from 13 calls to 3, and "kimi" walked nine originals in 8 calls.
- **Cost, recounted** (review M2): 3 to 8 API calls a query, 156 for the 28 (5.6 on average),
  counting the two searches, the uploads listings, the trees and the "by …" call (D19); 5 to
  180 KiB and 0.5 to 1.3 s each, before the header read, which is on the resolver bucket (§2.2).
  The first draft's "six calls" left the "by …" call out.

### 4.5 When it doesn't go well

| Case | The answer |
|---|---|
| No match at all (`NoMatch`) | "lmgw found no model called “<q>” that it can run. Names are usually written as on the model's page, with numbers as digits." (D18) |
| An exact match, but no GGUF of it anywhere (`NoRunnableFile`) | "lmgw found <name> on Hugging Face, but nobody has made a file of it that lmgw can run yet. Names are usually written as on the model's page, with numbers as digits." (D18) The "Did you mean" lines follow when there are any |
| Partial matches only (`LooseOnly`) | "Did you mean one of these?" with up to the hub's first page of them, by how many words matched, each a line with name, "by …" and likes |
| Many originals | One card for the first of the top author that fits, then "Other models with this name": every original found, by rank, each line opening its own card |
| An original with no GGUF anywhere | Left out of the card, listed under the others with "not available as a file lmgw can run yet" |
| Every upload gated | D20's sentence and links |
| Nothing fits (`NothingFits`) | The card of the original closest to fitting with "doesn't fit your graphics card" and its size against the card's ("None of the 7 models by … with this name fits your graphics card. The one closest to fitting is …"), "Other models with this name" open, and the file list one click away |
| GGUF files, none of 4 to 8 bits per weight (`NoFileInBand`) | "lmgw found <name> on Hugging Face, but none of its files is one lmgw picks by itself: every file is narrower than 4 bits per weight, … or the full 16- or 32-bit weights, …", the band in plain words, and the upload's address (D71) |
| A Hugging Face page that isn't a model's (`NotAModel`) | An account's: "huggingface.co/<name> is the page of <name> on Hugging Face, which lists its models, not one model. …"; a dataset's and a Space's say what those are; nothing sent (§4.1) |
| The search answered, the uploads' files not | "Hugging Face answered, but not when lmgw asked for the files of <name>. …" (`unreachable`), the statuses in the fold; one upload the hub can't answer for is passed for the next (D73) |
| No network, or the hub is down | "lmgw can't reach Hugging Face right now. Check the internet connection and try again." The reqwest error is in the fold |
| Rate limited | "Hugging Face asks lmgw to wait about 2 minutes. Without an account it allows 500 requests every 5 minutes; a Hugging Face token in Settings raises that." The time comes from `ratelimit_reset`, the quota and the window from the last `RateLimit-Policy` (`q=`, `w=`). The lists found before the limit stay. When the walk had found an original's size before the limit ran out, the answer is that `NothingFits` card, with a note: "Hugging Face's limit on requests ran out partway: lmgw checked 1 of the 2 models by … with this name, and the others weren't checked. It resets in about 2 minutes." (D76) |
| An address on another host | D124's sentence, nothing sent |
| A private repo | Its card, without "Send to a friend" (D124) |
| Header unreadable | D8 |

**The limits are the hub's, and they are shown.** A search usually costs 3 to 8 API calls (§4.4)
and the card's header read 1 to 3 resolver reads, so an anonymous person can run about 60 to 80
searches in 5 minutes. 3 to 8 is the typical case, not a bound *(WP4's review round)*: the two
searches, two calls for each original the walk reads, one more for each gated, weightless,
out-of-band or unanswered upload before the one used and for each further page of a tree, and
the display name's one or two. An author with hundreds of matching originals the listing gives
no parameter count for could cost hundreds (D17); the hub's remaining requests are the bound,
and an answer the limit stops partway keeps what was found (D76). lmgw keeps no counter of its own. It reads `RateLimit` (`r=` remaining,
`t=` reset) and `RateLimit-Policy` on every answer, and when `r` is below what one more search
costs, the Find button says "Hugging Face's limit resets in 40 s" and waits.

## 5. The card, and "Choose the file myself"

```text
┌───────────────────────────────────────────────────────────────────────────┐
│ Qwen3.6 27B                          by Qwen        fits your graphics card │
│ Reads pictures · thinks before it answers · 27 billion parameters          │
│ How long a conversation it can remember · context, 131 072 tokens          │
│ Uses 17 of the 18 GB free beside Kai's voice models                        │
│   With the voice models unloaded: Q5_K_M, 262 144 tokens · Use this one    │
│ 16.8 GB download, about 5 minutes on your connection                       │
│ Licence: Apache 2.0 ↗                                                      │
│ ▸ Why this file · Qwen3.6-27B-Q4_K_M.gguf from unsloth/Qwen3.6-27B-GGUF    │
│                                                                            │
│  Choose the file myself    Send to a friend                       [Get it] │
└───────────────────────────────────────────────────────────────────────────┘
  Other models with this name: Qwen3.6 35B A3B · fits …
```

- The figures above are illustrative. Real ones come from §3.
- Before the header read the marks say "fits by size" and the fit line "checking…" (D123).
- "Why this file" folds out §3.2's line, the upload and why it was chosen (D16), the companions
  (the projector D121 chose), the bits per weight, the file that remembers the whole conversation
  when it isn't this one (D7), and the header bytes read (D123). That is everything an
  expert needs to check the pick without opening the list.
- Under a machine check that isn't `ready`: the `MachineInCard` line (D11). Under the hold: D10's
  line. With no capacity: D9's question. Short of disk: D125's line.
- The licence links to the original's page on Hugging Face (D22).
- A private repo's card has no "Send to a friend" (D124).
- **"Choose the file myself"** opens `HfWizard` on the upload in its pick-only mode (D34), with
  K's pick selected and §3.3's marks: every file and class, the companions box, and "Use this
  file", which returns the file to the card.
- An embedder's card says "For searching your files (Knowledge)" in place of the conversation line
  and uses the same layout.

**The component.** `ModelFinder` lives in `lmgw-ui` (`widgets/model_finder.rs`). Its props:
`query: Signal<Option<String>>` (a host may fill the box), `compact: bool` (narrow hosts), and
`on_ready: Callback<ReadyModel>`. `ReadyModel` holds the id, the public name, the class and the
display name. The host decides what "Say hello" does. The Models page creates the thread and
navigates there. Part J's Chat empty state opens the thread in place. The component reads only
`/api/op/model_find`, `/api/op/hf_repo`, `/api/op/model_get`, the jobs frame and the machine
state, so the Chat never reads the admin plane itself (§8.3).

## 6. The flow's end

### 6.1 Get it

"Get it" posts `model_get { repo, file, target, measure, model_id? }` (D24). `measure` is the one
the card showed (D120). Before it, the card has checked the disk (D125). The answer has the
queued download ids, the companions, the engine line (machine check §5) and the job id. The card
turns into progress: one line per file with its bytes and time left, the engine line under it,
and "Cancel" (`hf_set action=cancel` per file, as today).

### 6.2 Set up (server-side, D25, D122)

A `model_gets` row holds the intent: repo, file, target, measure, model id, the download ids, and
its status. A `JobKind::ModelGet` job, key `model_get:<repo>/<file>` (one per file at a time),
runs it:
1. waits for its `hf_download` jobs. A failed one fails it with that download's sentence;
2. plans the file on disk with `local_model_plan`, now that every companion is there. This is the
   real plan, not the hub one, and §10 holds them equal;
3. takes the fit again against the measure the card showed, as it is now, and when it fits a
   shorter conversation than the trained one, sets `ctx_size` to that, and for an aux row also
   the tied ubatch and batch (D26), with the fit's note;
4. creates the row: `local_model_set action=create` for chat, `aux_model_set action=create` for an
   embedder, a reranker or a decision model (D29). The id is D28's. A plan that says a row already
   serves this GGUF (`configured_as` in sync) creates nothing and answers that row;
5. ends `done` with the row's id. It never starts the container (D27).

At boot, after the orphaned jobs are closed, each `model_gets` row that isn't finished gets its
job again: its downloads resume first (D122), and the job goes on from step 1. Its progress is in
the jobs frame, so the card, the Downloads page and an MCP caller see the same steps: downloading,
paused (metered), setting up, ready, or failed with its sentence. When the page was closed, the
model is simply there on the Models page.

### 6.3 Say hello

"Say hello" creates a thread (`POST /chat/api/threads { model_alias: <public name> }`,
`web/mod.rs:248`; the public name is `local_public_name`, D27) and opens `/chat?t=<thread>`. The
first message loads the model. Before that the Chat shows what it shows today for a model that
isn't loaded, so this needs no Chat change. A row that can't start meets the Chat's own handling:
the `gpu_setup` card, the hold's 503 or its fallback (the row has none unless someone sets one),
and admission's refusal.

An embedder ends "Ready for Knowledge · Make a knowledge base", linking to `/knowledge`.

## 7. Send this model to a friend

"Send to a friend" copies:

> Get Qwen3.6 27B in lmgw: open Models → Find a model and paste
> https://huggingface.co/unsloth/Qwen3.6-27B-GGUF/blob/main/Qwen3.6-27B-Q4_K_M.gguf

- **What it carries:** the repo and the file, in Hugging Face's public address (D31). The name in
  the sentence is the card's display name.
- **What it never carries:** the model id, any setting or planned value, the gateway's address,
  a token, the account, the revision this machine downloaded, anything from the request log. The
  text is built from the card's public fields only, and a test checks it against a row full of
  `*-demo` values.
- **When there is none:** a private repo (D124). The card says "This model is private to your
  Hugging Face account, so there is no link to share."
- **What it opens:** for the friend, the Find box plans that exact file for their own card. If it
  fits, the card shows it. If it doesn't, the card says so and shows the file of the same upload
  that fits (§3.2). Outside lmgw the link is the model's page on Hugging Face, which is harmless.
- `main`, not the commit: the friend gets the uploader's current version of the same file, the
  way the original download did.

## 8. Where it shows

### 8.1 Models page

A primary "Find a model" button in the page head, before "Add from Hugging Face", which stays
(`pages/models.rs:1156`). It navigates to `/models/find`, a page of its own under Models with the
`ModelFinder`, so a link to it works and the Overview's entry, part B and the share text can all
point there.

### 8.2 Overview, until part B

The Overview's empty state (no models yet) gets one line: "Run a model on your graphics card ·
Find a model by its name", linking to `/models/find`. Part B's checklist step "Choose a model"
replaces that line and links to the same route.

### 8.3 Part J's Chat empty state, later

The Chat is moving into `lmgw-chat-ui`, which can't depend on `lmgw-ui` (chat plane WP3b). So the
Chat gets the finder the way it gets the machine check, as a slot its host fills (`ChatMachine`,
`pages/chat_machine.rs:23`): a `ChatFinder { empty: Callback<(), AnyView> }` that the dashboard
fills with `<ModelFinder compact=true on_ready=…/>`, and that a paired device's host leaves empty.
Building that slot is part J's work, after WP3b. No WP here edits a Chat file.

### 8.4 The depths (part F, later)

The card is for everyone. The "Why this file" fold and "Choose the file myself" are the expert's
way in, and they are folds, so part F's depth switch has nothing to hide here. The marks in the
file list are on at both depths.

## 9. API

### 9.1 Ops and tools

| Op | Kind | Tool | Args | Answer |
|---|---|---|---|---|
| `model_find` | read | `lmgw__model_find` | `query`, `cursor?` (the hub's next page), `others_offset?`, `others_count?`, `did_you_mean_offset?`, `did_you_mean_count?`, `other_uploads_offset?`, `other_uploads_count?` (the lists' windows, 0 and 10 by default, D142) | `ModelFind` |
| `model_get` | write | `lmgw__model_get` | `repo`, `file`, `target?` (from the header when absent), `measure?` (`beside` by default, D120), `model_id?` | `ModelGetStarted` |
| `hf_repo` | read (existing) | `lmgw__hf_repo` | adds `fit` (default false, D36; the route also takes `unless_metered`, D68) | `RepoFiles` + marks |
| `local_model_plan` | read (existing) | `lmgw__local_model_plan` | `path` becomes optional; adds `repo` + `file`, the other way to name the file | the plan, plus `source: "hub"` and `fit` |

- `model_find` reads only, and no header (D123). It runs §4 to the card with size marks, and
  answers in about a second.
- `model_get` checks the file is in the repo and loadable by the class, as `hf_add` does, then
  queues through `hf_add` (with D121's projector) and starts the job of §6.2. Under a self-admin
  level that hides writes it is absent like every write.
- The MCP server's instructions (`mcp/ingress.rs:829`) say that the normal path to a new local
  model is `lmgw__model_find`, then `lmgw__hf_repo` with `fit=true` on the card's upload for the
  exact pick (it reads the file's header, 1 to 16 MB), then `lmgw__model_get`, then
  `lmgw__hf_downloads` until done, then `lmgw__local_model_test`. Step by step through `hf_repo`,
  `hf_add`, `local_model_plan` and `local_model_set` is the way to choose everything yourself.

### 9.2 Typed answers (`lmgw-api-types/src/model_find.rs`)

```text
ModelFind {
    query: String,
    card: Option<ModelCard>,          // none: see `answer`
    others: Vec<ModelChoice>,         // other originals, by rank
    did_you_mean: Vec<ModelChoice>,   // partial matches (D13), by matched words
    answer: FindAnswer,               // Card | NoMatch | NoRunnableFile | LooseOnly | AllGated
                                      // | NothingFits | Unreachable | RateLimited { reset_s }
                                      // | NotHuggingFace { host }
    sentence: String,                 // what the person reads for `answer`
    next_cursor: Option<String>,
    hub_budget: Option<HubBudget>,    // remaining, reset_s, quota, window_s (RateLimit, -Policy)
}
ModelCard {
    name: String, by: String, origin: String,      // "Qwen3.6 27B", "Qwen", "Qwen/Qwen3.6-27B"
    kind: CardKind,                                // Chat | Embed | Rerank | Decision
    upload: String, file: String, parts: u32,
    companions: Vec<HfCompanion>,                  // the projector of D121, the drafter
    can: Vec<String>,                              // "reads pictures", …
    parameters: Option<u64>,                       // the header's once read (D7)
    licence: Option<Licence>,                      // name and link (D22), shown, never a filter
    gated: bool, private: bool,
    origin_private: bool,                          // the original is private: a share names the upload (WP7, D109)
    download_bytes: u64, download_secs: Option<u64>, rate_from: RateSource,
    disk_free_bytes: Option<u64>,                  // D125
    fit: FileFit, whole_card: Option<FileFit>,     // D120: the shown measure, and the other one
    why: String,                                   // §3.2's line
    share: Option<ShareLink>,                      // url, text (§7); none for a private repo
    named_file: Option<String>,                    // the file a pasted address named, when file is it (WP7, D100)
    other_uploads: Vec<UploadChoice>,
}
FileFit { mark: FitMark, measure: FitMeasure, ctx: Option<u64>, trained_ctx: Option<u64>,
          uses_bytes: u64, capacity_bytes: u64, terms: FitTerms, beside: Option<BesideTerms>,
          bits_per_weight: Option<f32>, notes: Vec<String> }
ModelGetStarted { job_id: i64, model_id: String, public_name: String, target: String,
                  downloads: Vec<DownloadRow>, engine: Option<EngineDownload> }
```

`RepoFile` (`downloads.rs:29`) gains `fit: Option<FileFit>`, and `RepoFiles` (`:11`) gains
`recommended: Option<String>`, `card: Option<CardMeasure>` (capacity, headroom, budget or not,
admission on or not, outside use, residents) and `header_bytes_read: u64`. All are
`#[serde(default)]`, as the DTOs are. `FileFit` is WP2's, so WP3 and WP4 share it.

### 9.3 Docs touch points

Every new op gets its op name (`web/op_names.rs`), an `OpDoc` in `ops-downloads`
(`openapi/ops/table/downloads.rs:11`) with the tool's description, and its typed response schema.
`GET /api/hf/repo`'s `DocRoute` (`openapi/registry.rs:24`) gets the `fit` query parameter in its
query struct, with its false default. `GET /api/local-model-plan` gets a query struct of its own
in place of the shared `InspectQuery` (`web/api.rs:399–403`): `path` optional, `repo` and `file`
added, one of the two forms required. Its `DocRoute` (`openapi/planes/dashboard.rs:205`) and the
`lmgw__local_model_plan` tool's input schema change with it. No new HTTP route: everything new
goes through `/api/op/*`. No new `x-lmgw-*` header. `/v1` is untouched.

## 10. Tests

**Unit, no network:**
- *The header from a stream* (`gguf`): the same bytes through `read_header` on a file and
  `read_header_from` on a stream give equal `GgufMeta`; a stream cut in the metadata, in the
  descriptors, and exactly at the data start (EOF, EOF, success); a declared length past `<total>`
  (refused, as on disk); the stream dropped at the data start (no byte of tensor data read, counted
  by a test reader); the parameter count and the repeating layers' bits per weight from the
  descriptors of a recorded header.
- *The projector rule* (D121): F16 and BF16 and F32 in one directory give the smallest of F16 and
  BF16; only F32 and Q8_0 give the smaller; one projector gives it; `hf_add`, the hub plan, the
  disk plan and `model_get` give the same file for the same listing, and `hf_add` queues only it.
- *Split weights* (F1, as the separate fix leaves it): a hub plan of a split file charges the
  tree's sum, as the disk plan does.
- *Fit* (§3): the trained context fits; a shorter one fits (the largest, by the planner's search);
  nothing fits at one token; the beside measure with outside use and residents, without a device
  reading; the hold on (marks unchanged, the line); no capacity (`NoCard`); admission off (fits,
  with the note); the projector and drafter counted; an aux fit moving ubatch and batch with the
  context.
- *The pick* (§3.2): rule 1, rule 2 at 32 768, the trained context below 32 768, below 4 bits per
  weight passed over, above the Q8 class passed over (D7), no file at all; ties by
  size; the whole-card and the beside picks differing.
- *The estimate band* (§3.3): the conservative estimate is never below the exact one over a
  recorded repo's headers; the band holds exactly the files whose exact fit could differ.
- *The ranking regression set* (§4.4, WP4's entry gate): the 28 queries over recorded hub answers
  in `crates/lmgw-core/tests/fixtures/hf_find/` (D38): the card's original per card size, the
  upload, the others' order, the answer kind; the maker's own GGUF folded in; a gated upload
  behind an open one; "gemma four" giving `NoRunnableFile`; the walk keeping author, kind and
  match; the pass-over making no call.
- *The query parser* (§4.1): each Hugging Face address shape, `hf.co`, an address inside the
  share sentence, `owner/repo`, a name; an address on another host answered locally, with a test
  client that fails on any request.
- *Share text* (§7): built from a card whose row and settings are full of `*-demo` values and
  `.example` addresses, it contains none of them; a private repo's card has no share link.

**Integration** (wiremock hub with recorded answers, a fake CDN serving recorded header bytes with
`206` and `Content-Range`, `tests/it`):
- `model_find`: a card with size marks and no header read; the hub's 429 (`RateLimited` with the
  reset and the policy's quota); a 401 on every upload (`AllGated`); no route to the hub
  (`Unreachable`); the token sent to the hub's hops only, never to the CDN, and none when unset;
  a private repo in a token's answer (no share link).
- `hf_repo` with `fit`: marks, `recommended`, the header bytes read, the CDN cutting the stream
  (`BySizeOnly`); `fit` absent reading no header.
- `get_with_commit` with a Range header: the Range on the CDN's hop only, the hub hop's
  `x-linked-etag` and `x-linked-size` returned.
- *A download across a restart* (D122): the process stopped mid-download, a new one booted on
  the same DB: the download resumes from its `.part` with a Range from the byte it reached and
  ends with the same bytes as a straight download; a changed hash restarts it from byte 0 and says
  so; a CDN answering `200` to the Range restarts it; a metered connection leaves it paused;
  cancel deletes the `.part`.
- `model_get`: downloads, a plan from disk equal to the hub plan of the same bytes (except
  `source`), the row created with the fitting context and the fit's note, never started (no
  container spawned, the fake Podman records none); an embedder into the aux directory and an aux
  row with the tied batch at the fitted context; a failed download failing the job; a second
  `model_get` of the same file joining the first; an existing row serving the file (nothing
  created, that row answered); a public name that's taken in another class (`-2`); *a restart
  between the downloads and the set-up* (the job re-attached at boot, the row created once); *a
  full disk* (the free-space warning in the answer, and a download failing with ENOSPC failing
  the job with its sentence, the `.part` kept for a resume).
- The OpenAPI document lists the new ops with their schemas and the changed `local-model-plan`
  query, and the op-name and tool-catalog checks pass.

**UI** (`ui-drive` against a dev instance with the fake hub; never the installed app):
- the finder from empty to "Say hello" in four presses (Find, Get it, waiting, Say hello) without
  visiting Settings; the card's size marks first, then the header's;
- the marks and the preselected file in "Add from Hugging Face"; "Choose the file myself" in the
  pick-only mode returning the file to the card;
- the machine line under a fake `needs_fix`, the hold line, the rate-limit wait on the button,
  the beside pick with its whole-card line, the inline card-memory question, the disk warning, a
  private card without a share link;
- `ui-matrix` runs `/models/find` with the card, the progress and each `answer`, at 125 %.

## 11. Work packages

In landing order, each landing on main before the next is reviewed. Each ends with the project's
checks. WP3, WP4 and WP5 can be built side by side after WP2, but they all add to
`web/op_names.rs`, the op dispatch in `web/api.rs`, the `OpDoc` table and the MCP catalog, so they
land in sequence, WP3 then WP4 then WP5, each rebased on the one before. No WP edits a Chat file.

- **WP1 — The hub reads, the projector rule and resumable downloads (M; needs nothing;
  D40–D49).** `gguf::read_header_from` over a stream with a total length, `read_header` on top of
  it, and the descriptors' parameter count and repeating-layer bits. `get_with_commit` taking
  request headers for the CDN's hop and returning the hub hop's `x-linked-etag`,
  `x-linked-size` and commit. `hf::remote_header(repo, file)`: the Range read with the token rule,
  the stream dropped at the data start, the in-memory cache by `lfs.oid` (D4), which `HfFile`
  gains. The projector rule (D121) as one function used by `hf_add` and the disk plan
  (`modelinfo.rs:1388`), which fixes bug H1 in today's code. An interrupted download kept and
  resumed by Range at boot, the hash on `hf_models`, the metered pause (D122's download half,
  bug H2). Unit and wiremock tests, the projector rule's and the restart's included.
  **Built (2026-11-04).** `gguf::read_header_from`, `GgufMeta::{tensor_elements, data_start}`,
  `ModelSummary::{parameter_count, layer_bits_per_weight}` (D49); `get_with_commit` with the
  CDN's headers and `HubHop` (D40); `HfFile::lfs`/`oid`; `hf::remote_header` with
  `AppState::hf_headers` (D43, D44); `hf::pick_projector` in `hf_add`, the chat and aux disk
  plans and the capabilities note (D121, D41); migration 0097 `hf_models.oid`, the `.part`
  continued by Range, `jobs::hf_download::resume_at_boot` in place of the boot's re-queue, the
  metered pause through `machine::connection_metered`, the Downloads page's "paused" chip (D42,
  D45 to D48). *Tests:* `gguf::stream_tests` (a stream and a file equal; cut in the keys, in the
  descriptors and at the data start; a length past the total; no byte past the data start taken;
  the parameter count and the layers' bits), `hf::projector::tests` (16-bit over wider, the
  smallest without one, ties by name, an unknown size, the width word), `hf::resolve::tests`
  (the Range on the CDN's hop only, the hop's three headers as recorded), `hf::remote::tests`
  (`Content-Range`), and `tests/it/hf_resume.rs`: the header read with its Range and the
  cache hit costing no request, a refused and a cut read, one projector of three for `hf_add`
  and the disk plan, a restart on the same database resuming from the `.part`, a changed hash
  starting again, a `200` to the Range starting again, the metered pause and its Resume, and
  cancel deleting the `.part` and its hash. The hub plan and `model_get` sides of the projector
  rule's test are WP2's and WP5's, as those callers don't exist yet.
  **Review round (2026-11-04, D128 to D135).** The hash recorded after the `.part` is emptied
  (D128); strings read in steps through `try_reserve` and a read off the network bounded by
  `MemAvailable` with its sentence (D129, `gguf::read_header_within`,
  `MachineChecks::memory_available`); the length cross-checked with `x-linked-size` and the
  sha256 checked while streaming and over a kept `.part` read once before a resume (D130); a
  split part's count as its share, `gguf::params` with `whole_parameter_count` (D131); a `416`
  over a whole `.part` finished (D132); `nmcli monitor` while rows are paused, their resume when
  the connection isn't metered, Cancel on a paused row (D133, `machine/netwatch.rs`); the stall
  bound on downloads raced against the cancel (D134); `DownloadRow::kept_bytes` and "N kept for
  Retry" (D135); the capabilities note's projector by header role and followed size; the header
  cache's key lowercased on store and its in-flight guards pruned. The resume helpers are
  `jobs/hf_download/resume.rs`. *Tests:* `gguf::stream_tests` (a 19 GB string declared under a
  20 GB total ending at the stream's end; a bound stopping a 1 GB string, a 5 GB byte array and
  an array of strings that reaches it, before their bytes are taken), `gguf::params::tests` (a
  part's share, the whole count from its parts, a short, doubled or mixed set giving none), and
  in `tests/it/hf_resume.rs`: an old `.part` under the new hash (the D128 window) failing its
  sha256, deleted, and a Retry downloading it whole; no hash recorded for a `.part` that couldn't
  be emptied; a fresh download with the wrong sha256 deleted; no `Content-Length`, the hub's size
  taken, the whole body done and a short one kept with its hash and shown as kept; a short body
  failing and Retry going on from its end; a `206` from another offset starting again; a `416`
  over a whole `.part` finished with one request, and one of other bytes deleted; a hop without
  a hash starting a kept `.part` again; a header declaring more than the stated memory stopping
  with its sentence; a paused row asked again on a change while metered and going on by itself
  once not, its watch closed after; Cancel on a paused row; the capabilities note naming the
  projector the plans take (a chat model named like a projector passed over, a symlink sized as
  its target).
- **WP2 — One plan input, the fit and the pick (L; needs WP1, and F1's separate fix on main;
  D50–D59).** First, the fit checks' disk reads (`plan_fit.rs:86`, `:101`, `aux_plan.rs:383`)
  move into `PlanFiles`, with a non-split row's footprint byte-for-byte unchanged. Then
  `PlanFiles` from the hub (§2.3), `local_model_plan`'s `repo` + `file` form with its own query
  struct, its `DocRoute` and its tool schema (§9.3). The `Fit` module (§3.1) over `plan_fit`'s
  card and search, with both measures (D120) and the aux fit's tied batch. The pick (§3.2, as D7
  is ruled). `FileFit` and the other wire types of the fit. The download-rate estimate weighted
  by bytes, with the engine image (D23). The tests of §10's header, fit, pick and band groups.
  **Built (2026-11-04, D50 to D59, D136 to D139).** `PlanFiles` with `from_disk` and
  `plan_files::hub::from_hub` (D50, D51, D52, D54), the chat and aux plans over it
  (`plan_from`, `chat_plan`, `aux_plan`), `local_model_plan_hub` and `local_model_plan_of`,
  `GET /api/local-model-plan`'s `PlanQuery` with its `DocRoute`, and `lmgw__local_model_plan`'s
  `repo` and `file`; `plan_fit::Sizer` and `largest_fitting` shared by the warning and the fit;
  `fit::{CardNow, fit, by_size, fitted_params, note}` with `VramScheduler::on_card_now` (D55,
  D57, D136, D139); `pick::{pick, full_context_for}` (D7, D53, D56); `hub_fit::{file_fit,
  pick_both}`, a file's fit from the hub with D8's fallback and the two picks; `hf::rate` (D23,
  D58); the wire types in `model_find.rs` (D137). *Tests:* `tests/it/plan_pin.rs` (today's
  plans pinned before the move: chat with companions, split, aux, no card); `modelinfo::fit`'s
  (the trained context fits; a shorter one, the plan's own number; nothing at one token; beside
  with outside use and a resident, and without a device reading; the hold's line and unchanged
  marks; no card; admission off; the projector and drafter counted; an aux fit moving ubatch and
  batch, and a chat one not; the size marks; the row's note); `modelinfo::pick`'s (rule 1; the
  32k fallback; a trained context below 32k; the 4-bit floor; the Q8 cap over BF16 and F32; the
  fold's "remembers all 262 144 tokens"; no file or size marks only; ties by name; the two
  measures differing; token spacing); `gguf::params`'s summaries adding up count and width;
  `hf::rate`'s (weighted by bytes, a resume's moved bytes, the engine and the header-read
  fallback, the sentence); `vram::beside`'s residents with their peaks; `plan_files::hub`'s listed
  split bytes; and `tests/it/plan_hub.rs` against a mock hub and CDN (the hub plan equal to the
  disk plan of the same bytes for chat, split and aux, with one projector read of two; a split
  file's whole count and width read from its second part; the pick on the whole card and beside
  a program's share, unchanged under the hold; a cut header marked by size and passed over; the
  estimate never below the exact footprint). *Fix round (2026-11-04, after an independent
  review):* the width over the quantized matrices, sized by type (D140); a loaded row of the
  planned file left out beside it (D55); a split set read only as part 1 declares it, else by
  size (D54); a header read from the cache counted as no bytes (D123); the hub plan's location
  rule by the asked class (D51); unknown listed bytes never a fit, and the real reason in the
  fold (D8); a CPU plan fitting without a card and a header without a context getting no
  "(0 tokens)" (D57, D7); the engine's layers already here left out and a resume's hash pass
  left out of the rate (D23, D58); the plan's messages neutral to `path` and `gguf_path`; image
  plans said to carry no `source` or `fit` (D137). *Its tests:* `gguf::params`'s width with F32
  norms, biases, an F32 MoE gate and padding (8.5), an F32 file (32), and the real
  `bge-small-en-v1.5-q8_0.gguf` read from disk (8.5; skipped where the file isn't);
  `gguf::tensors`' type sizes; `modelinfo::pick`'s Q8_0 with F32 norms by rule 1, rule 2 and the
  fold; `modelinfo::fit`'s loaded row left out, a CPU plan with and without a card, a split set
  by size and unknown, unknown bytes; `hf::rate`'s hash pass and layers here, and the upper
  bound's note; `plan_files::hub`'s unknown listed bytes; `tests/it/plan_hub.rs`'s quants with
  F32 norms (the Q8_0 at 8.5 and picked), an embedder planned for chat equal to its disk plan
  in the chat dir, a split set whose count disagrees or whose part is unlisted read once, a
  second plan counting no header bytes, and an image target marked by size with its refusal;
  `tests/it/plan_pin.rs`'s five more pins and every pin's fit against its warning (D50, D136).
  *Not yet:* the band of exact reads near the card's
  edge and `hf_repo`'s `fit` (WP3, D138); no caller writes `fitted_params` or `FitNote` yet (WP5,
  D139); the download estimate has no caller yet (WP4's card); an image plan's fit on the card
  (D137). *Unverified:* no real Hugging Face
  file went through the hub plan (the mock CDN serves synthetic headers; WP8's live round
  compares a real hub plan with its disk plan: *WP8 did, for two real files, equal*); `hf::rate::engine` and its read of the image
  store's layers (`MachineChecks::layers_here`) were type-checked, not run against Podman or a
  registry; the estimate's never-below argument was tested on synthetic widths, not recorded
  headers; of the ggml type table only F32, Q8_0, Q4_K and Q6_K were checked against real files
  (the two bge files), the rest is `gguf-py`'s table as copied.
- **WP3 — Part C: marks and the recommended file in the file list (M; needs WP2; lands first of
  WP3 to WP5; D60–D69).** `hf_repo`'s `fit` with its false default (§3.3's estimate band, the
  exact reads, the header bytes read), `RepoFile.fit`, `RepoFiles.recommended` and `card`, the
  tool's description, and the `DocRoute` query. In `HfWizard`: the marks column, the preselected
  file (no download before the button), the exact read on click, props for an initial repo and
  file, and the pick-only mode with "Use this file" (D34), which WP6's "Choose the file myself"
  uses. `wizard.rs:173`'s comment changes: an earlier pass picked nothing because nothing could
  tell which file fits.
  **Built (2026-11-04, D60 to D65).** `hf_repo`'s `fit` (`ops::hf_repo`, `GET /api/hf/repo`'s
  `HfRepoQuery.fit` and its `DocRoute` text, `lmgw__hf_repo`'s `fit` and description) over
  `modelinfo::hub_fit::marks::repo_marks`: the reference (D60), the estimates, the band (D61) and
  the pick's reads (D62), with `file_fit_planned` and `modelinfo::plan_sizer` shared with the
  plan; `ModelSummary::input_tensors`, `GgufMeta::tensor_elements_of` and
  `PlanFiles::layer_elements` for the two bounds; `RepoFile.fit` and `estimated_from`,
  `RepoFiles.recommended`, `picks`, `card`, `header_bytes_read` and `fit_notes`, `RepoPicks`
  (D63). In `HfWizard` (`pages/wizard.rs`, the words in `pages/wizard/marks.rs`): the marks, the
  "recommended" and "checking…" chips, the line above the list, the preselected file, the exact
  read on choosing, `initial_repo`, `initial_file`, `initial_target` and `on_pick` with
  `PickedFile` (D64, D65); the comment says why an earlier pass picked nothing. *Tests:*
  `modelinfo::hub_fit::marks`'s (when a file is read for the pick; the 4-bit floor by bytes),
  `pages::wizard::marks`'s (every mark's words and tooltip, no marks without a card), and
  `tests/it/hf_repo_marks.rs` against `plan_hub`'s mock hub and CDN: without `fit` nothing is
  read, over HTTP too, and `fit=true` reads; over 42 cards (40 to 200 MiB, empty and a third
  held by another program) every mark is the file's own header's, a file read is marked as its
  plan is, an estimate's footprint is never below it, each header is read once, and the
  recommended file and both picks are what every header gives, with fewer reads than files; a
  file near the edge read and one past it not (the band); a cut reference marked by size and
  the next read, the files smaller than the pick never read; no card reading nothing; the image
  class reading nothing; the hold said once without changing a mark. `ops_downloads` pins the new
  keys.
  **Fix round (2026-11-04, D66 to D69, D61/D63/D64/D65 revised).** One card reading per answer,
  references per set of companions, the typed `hf::ReadError` that stops at the hub's refusal and
  counts a failed read's bytes (D66); the pick side's and the cap side's bounds and the
  reference's label order (`hub_fit/marks/bounds.rs`, D67); `fit=unless_metered`, `RepoFit` and
  `RepoFiles.metered`, the page's "Check exactly" (D68); the band's elements every part's
  (`gguf::InputTensors`, `PlanFiles::host_input_elements` and `overhead_bytes`, D61); the list's
  card and notes said once (`marks::compacted`, D63); the chip and the line of a whole-card pick
  and `who` (`pages/wizard/marks.rs`, D64); the page's reads waiting for the marks and a Fetch
  clearing the list before it asks (D65). *Tests:* `tests/it/hf_repo_marks/sweeps.rs` runs the
  sweep over a split reference whose head is in its second part, subdirectories with and without
  a projector, a tied head with per-layer embeddings, 2- and 3-bit layers beside F32 and F16
  embeddings, a mixture of experts, an aux repo, two of lmgw's models on the card (one serving the
  repo's first file) and a split set whose header declares three parts;
  `hf_repo_marks/refusals.rs` holds a 401, a 429 and an unreachable CDN to one request each, and
  a metered connection reading nothing until `fit=true`; the cut-reference test counts the cut
  read's 40 bytes; `bounds`' and `marks`' unit tests hold the label widths, the cap side, the
  companion sets and the compaction; `pages::wizard::marks` holds the whole-card pick's chip and
  line and `who`. *Read cost* on `unsloth/Qwen3-32B-GGUF` (26 weights files, no token, a dev instance on a 24 GiB test GPU, about 2 GiB held by other programs both times): before the fix round 12
  weights headers (D62 said 13; the answer saved then marks 12 as read), 72.7 MB, an answer of
  33.5 KB; after it the same 12 headers, 73.5 MB in 11.1 s (what a header read receives past
  the header varies with the chunks), an answer of 24.3 KB. Every one of the 12 is a band read
  (D61): on this card the pick's tightenings (D67) leave no read out, and D69 is the lever left. *Not yet:* the reads are one after another; D69's "near the edge"
  marks are the owner's call; WP6's "Choose the file myself" is the first `on_pick` host.
  *Unverified:* the marks were seen on a dev instance against the real hub for
  `unsloth/Qwen3-32B-GGUF` and `unsloth/Qwen3-8B-GGUF` (real headers through the planner, the
  fit on the real card) and nowhere checked against a real start of the files; the pick-only
  mode has no host yet and was not driven; no aux repo was listed with marks against the real
  hub; the metered path ran on a stated machine, not a real metered connection, and "Check
  exactly" was not clicked on one; D67's twice-the-header margin is a choice, not measured
  across a repo's real headers.
- **WP4 — Search by name (L; needs WP2; lands after WP3; D70–D79).** *Entry gate:* the 28 queries
  of §4.4 recorded first as fixtures with a `scripts/hf-find-record.sh` that records them again
  from the live hub (no token; it writes only under `crates/lmgw-core/tests/fixtures/hf_find/`),
  and the matching and ranking code passing them, the table's cards as the expectations (granite's
  miss recorded as the known one), before the op is built. Then `hf::search` (the two calls, the
  cursor, `RateLimit` and `RateLimit-Policy` read on every answer), the query parser with D124's
  local refusal, matching (D13), originals (D14), ranking (D15), uploads (D16), the walk of §4.3
  (D17, D126), the "by …" call (D19), `private` (D124), `model_find` with its `OpDoc` and op name,
  `lmgw__model_find`, the DTOs of §9.2 (`ModelFind`, `ModelCard`, `ModelChoice`, `ShareLink`),
  and the integration tests.
  **Built (2026-11-04, D70 to D79).** The entry gate first: `scripts/hf-find-record.sh` and the
  recording in `tests/fixtures/hf_find/` (D78), recorded 2026-11-04 (144 reads), and the regression set
  passing it. `hf::search` (`hf/search.rs`, `hf/search/hub.rs`): the `Hub` seam with `LiveHub`,
  `Reads` (the calls counted, `RateLimit` and `RateLimit-Policy` read from every answer, nothing
  sent once the hub said none are left, D76), the two searches at the hub's page size with their
  cursor (D12, D70), the uploads listing, the tree, one repo's listing and the display name
  (D19). `find` (`find.rs`, `find/{query,words,rank,walk,card}.rs`): the query parser with
  D124's local refusal (§4.1), the words and their match (D13, D18), originals with the maker's
  GGUF folded in (D14), the rank and the walk's set with its chat signal (D15, D126), the plain
  uploads in D16's and D20's order (D73), the walk with its pass-over and the size pick (D17,
  D71, D72), an address straight to its repo (D77), and the answer (`find::card::answer`): the
  card with both measures' size marks through WP2's `fit::by_size` and `CardNow`, the companions
  by D121's rule, "reads pictures", the licence, `private` and the share link (D124), the
  download estimate (D23, D79), the free disk (D125), the others, "Did you mean", the budget and
  the sentences of §4.5 (D74). `model_find` (`ops/model_find.rs`) with its op name, its `OpDoc`
  in `ops-downloads` and `lmgw__model_find` (read-only, closed arguments); the wire types in
  `model_find.rs` (D75). *Tests:* the units of `find::{query,words,rank,walk,card}` and
  `hf::search` (every address shape, an address inside the share sentence, another host; D13's
  words; an upload of a matching model no original; the maker's GGUF folded; weights before
  likes; plain uploads; the band, the room and a split file missing a part; the cursor; the
  rate-limit headers); `tests/it/hf_find/regression.rs`: the 28 queries on both cards against
  §4.4's table (55 of 56 cards as the table has them, granite's miss recorded as
  the known one; on 8 GB "nemotron" gives `nvidia/Nemotron-Orchestrator-8B`, 601 likes, where the
  table has `NVIDIA-Nemotron-Nano-9B-v2`, 532: the spike's first 100 repos by downloads didn't
  hold it, the hub's whole page of D70 does, and D15's likes rank it first; accepted by the owner 2026-11-04, ruling 2), "gemma four" searched as typed with the digits hint, the
  pass-over costing no call, the maker's own GGUF as the upload; `tests/it/hf_find/op.rs` against
  a mock hub: a card by size with no header read, the token on the hub's reads when set and
  none when not, a 429 and a window running out said in words, every upload gated, a private
  repo without a share link, no route to the hub, an address elsewhere sending nothing, a file
  address, no match, the cursor, and the tool.
  *Not yet:* the MCP instructions' normal path (§9.1) is WP5's; the band of exact header reads
  is WP3's `hf_repo` `fit`, which the page asks after the card (D123); the uploads listing's
  later pages aren't read (D70).
  **Review round (2026-11-04).** *Built:* compression for the hub's metadata reads only (D141, `hub_http`; every other client
  without it, `identity` on every hop of a file); the walk's real cost said in D17, §4.5 and the tool's description, and
  a limit used up partway keeping what was found (D76); `NothingFits` on the original closest to
  fitting (D17); an upload with no file in the band passed, and `no_file_in_band` (D71); an
  upload's or an original's error passed with a note (D73); the addresses: anywhere in a word
  (D124), the model search page as a name, the pages that aren't a model's (`not_a_model`), a
  non-GGUF file of an original's repo, a file not on `main` or not weights, a named file on the
  whole card and a narrow one that fits (D77); the share link encoded (D31); `by_user` (D19);
  the lists' windows, the card's other uploads too, in the op and the tool (D142); the recording's field check (D78). The recording
  needed no new read and no new field, so it wasn't recorded again; over it the 56 cards are as
  before but for "deepseek v4 flash"'s `NothingFits` card (D17). *Tests:*
  `find::query` (an address inside a word, a digit before the scheme, the userinfo, case and port,
  the hub's pages), `find::card` (the share link's encoding read back, the windows, a named file
  that fits only the whole card), `hf/resolve.rs` and `hf_resume.rs` (`identity` on the hub's hop
  and on every CDN request of a download), and `hf_find/op.rs`: a limit spent at the third read
  (three requests sent, the passed-over model's card, the note, the lists; spent before anything
  was found: `rate_limited` with the lists), a tree 404 passed for the next upload and a 503 with
  none after it, a first upload with no file in the band and an only one, the addresses (a
  Markdown link, `config.json` and a safetensors link of the original, a file on revision `v1`, a
  Q2_K that fits a 5 GiB card), the hub's pages sending nothing and its search page searched, the
  windows over 25 others and the card's uploads in the op and the tool, a gzipped hub; the
  regression set prints each answer's size with the default windows and with every entry. *Not yet:* a cheap size bound for
  an original without a parameter count (a GGUF-only or `pytorch`-only one): the listing's own
  fields bound no file from below, and `expand[]=gguf`, the hub's read of a GGUF's parameter
  total, costs about 7.6 KB a repo (its chat template, Findings kept for later), so such an
  original still costs its two calls; reading the hub's documented page maximum live (D70);
  brotli (D141); a later window that skips the walk.
  *Unverified:* the live hub was read only for the recording (2026-11-04, no token); a private
  repo and a token's answer were tested against the mock hub only; the engine's part of the card's
  time is WP2's `hf::rate::engine`, still not run against Podman or a registry; the hub's gzip was
  tested against a mock (the live hub's `Content-Encoding` on `/api/models` wasn't read in this
  round), so how much smaller a live page comes isn't measured anew (D70's spike: about 150 KB
  of 600 KB).
- **WP5 — Get it, server-side (M; needs WP2; lands after WP4; D80–D89).** The `model_gets` row
  and `JobKind::ModelGet` (`jobs/model_get.rs`): wait, plan, fit to the card's measure, create,
  done (§6.2), re-attached at boot (D122's Get-it half). `model_get` with its `OpDoc`, op name and
  `lmgw__model_get`. The id and the public-name check across `/v1` (D28). The aux path with the
  tied batch (D29, D26). The fit's note on the row and the model editor's "Fit again" (D26, as
  ruled). The free-disk check (D125). The MCP instructions' new normal path (§9.1). The
  integration tests, the restart and the full disk included.
  **Built (2026-11-04, D80 to D89).** Migration 0098 (`model_gets`, `model_fit_notes` and its
  four triggers, D80) with `store::model_gets`; `JobKind::ModelGet` and
  `jobs::model_get::{start, reattach_at_boot}`, called by the boot after
  `hf_download::resume_at_boot` (D81, D87); `ops::model_get` and `ops::model_fit_again`
  (`ops/model_get.rs`: the id and its suffix, D28, D84; the class from the header, D83; the
  files less what is on disk, D82; the free-disk check, D89); `ops::with_companions`, shared
  with `hf_add`; `modelinfo::refit::{for_get, for_row}`, the writer of `fit::fitted_params`
  and `fit::note`, with the aux re-plan (D85, D86, D88); the full-disk sentence in
  `jobs::hf_download` (D89); the wire types in `lmgw-api-types/src/model_get.rs`; the op
  names, the dispatch, both `OpDoc`s, `lmgw__model_get` and the MCP instructions' normal path
  (§9.1); the fit note and "Fit again" in the chat and aux editors
  (`pages/model_editors/fit_again.rs`). *Tests:* `tests/it/model_get.rs` against a mock hub and
  a Range-capable CDN, with a Podman that records every call: a chat get (one projector of two
  and the drafter, the disk plan's parameters the hub plan's, the row at the fitted context with
  its note, nothing started); an embedder by its header into the aux dir with ubatch = batch =
  the fitted context; a failed download failing the get with its sentence; a second get
  joining the first; a row already serving the file answered, nothing downloaded again; a
  public name taken in another class (`-2`); a restart mid-download (the `.part` resumed by
  Range, the get going on, the row created once) and one after the create (answered, not
  created again); a get larger than the free disk warning in its answer; `ENOSPC` (`/dev/full`
  behind the `.part`) failing the get with the full-disk sentence and keeping the `.part`; Fit
  again on a larger and a smaller budget (the changes shown, a stale confirm writing nothing,
  the confirmed save, the note gone and back) and an editor save of another context dropping
  the note; the id rule. The UI's note and change lines have unit tests.
  **Review round (2026-11-04, D80, D81, D84, D86, D87, D88 revised).** The row first and the op's
  writes detached; the jobs frames of out-of-transfer status writes and the stage sent at once;
  a cancel limited to the get's own downloads; the failure sentence's "Get it again"; a
  shutdown leaving the get running; a start that never takes another get's job; the D87 lookup
  in the op and the missing note written at re-attach; `model_get_status` with its `OpDoc`, op
  name, `lmgw__model_get_status` and wire types, and the MCP instructions' step 4 on it; the
  pass-through names in D28; the aux kind on the row; Fit again's confirm by "still fits", its
  ties for aux rows only, and its save detached; the note on the row's read, the editors' labels
  and the note read again after the editor's save. *Tests* (`tests/it/model_get/review.rs`): a
  paused download's Cancel and untrack reaching the get waiting for it; two gets sharing the
  projector, the first cancelled and the second finishing with it; a get's own Cancel and a
  download's Cancel; two gets of one file at once; a request dropped while the op queued; the
  status read from running to done; a name taken during the download, suffixed at the create; a
  given id under a pass-through prefix and one a bare upstream lists, refused; Fit again's
  confirm with the budget a little larger (saved), smaller than the shown context (outdated) and
  the row edited meanwhile (outdated), a chat row's batch unmoved; Fit again on an aux row (its
  tied batches with its context, the note on the row's read gone); an aux re-plan under a
  projector floor keeping the floor at the fitted context (D86, revised) and Fit again growing
  past it; the restart test now also cut before the note's write;
  and a unit test that every tool and argument the admin instructions name is in the
  self-admin catalog (WP4's `lmgw__model_find` marked as ahead of main until its
  rebase).
  *Not yet:* Fit again has no MCP tool, and the editors offer it only for a row with a note; the
  free-disk check leaves the engine image out (D89); a reranker's and a decision model's "Ready
  for" lines are the job's `next_step` text only (the card's words are WP6's); a Retry on the
  Downloads page of a failed get's download finishes that download only, the get being started
  again by Get it again. *Unverified:* no real model went through a get (mock hub, synthetic
  headers; *WP8 ran two real gets, one cut by a quit and re-attached*); the editors' note and button were type-checked and
  unit-tested, not looked at in a window; a real full disk was played by `/dev/full`, not met;
  that hyper drops the handler of a request whose client went away was taken from the
  `crate::detached` record, and the dropped-request test passes whether it does or not; the
  pass-through check against a real provider's catalog was not run (a mock catalog only).
- **WP6 — The finder in the dashboard (M; needs WP3, WP4, WP5; D90–D99).**
  `widgets/model_finder.rs` (`ModelFinder`, its props and `ReadyModel`), the `/models/find` route,
  the Models page button, the Overview's empty-state line, the size marks first and the header's
  after (D123), the metered "Check exactly", progress from the jobs frame, "Say hello" through
  `POST /chat/api/threads` under the public name, the machine, hold, card-memory and disk lines
  (D9, D10, D11, D125), the beside pick with its whole-card line (D120), "Choose the file myself"
  into `HfWizard`'s pick-only mode, the "Other models" and "Did you mean" lists, and the
  rate-limit wait. UI drives.
  **Built (2026-11-04, D90 to D99).** `widgets/model_finder.rs`: `ModelFinder` (`query`,
  `compact`, `on_ready`, and `on_search` for a host that keeps the query in its address),
  `ReadyModel`, the search, the header's marks with `fit=unless_metered` and "Check exactly"
  with `fit=true`, the rate limit's wait on the Find button with its countdown, and "Show more";
  its children `words` (every line's words), `shown` (the card's file from the card by size, the
  header's marks and a file chosen by hand, D120, D123), `card` (the card, the fold, the hold,
  machine, memory and disk lines, "Use this one instead", "Choose the file myself" into
  `HfWizard` with `on_pick`), `no_card` (D9's question, D94), `progress` (Get it, the progress
  from the jobs frame, Cancel, the end and "Say hello", D90 to D93) and `lists`; `assets/finder.css`.
  `pages/models/find.rs`: `/models/find` (`?q=`, `?hf=`), whose "Say hello" posts
  `/chat/api/threads` under the public name and opens `/chat?t=<id>`. The Models page's primary
  "Find a model" and the Overview's empty-state line (D98). `PickedFile` gains `size_bytes` and
  `quant`; the wizard's `marks::{who, who_of, mark, tip}` are shared with the card.
  `scripts/ui-matrix.py` and `scripts/drive/inapp-panics.json` list `/models/find`. *Tests:* unit
  tests of `model_finder::words` (the id, parameters and the can line, the chip's words by
  measure and state, the context, uses and doesn't-fit lines, times, the disk line, the limit's
  wait, the budget in MiB), `shown` (by size with the other pick and back, the marks replacing
  the pick and leading with beside, nothing beside on the whole card, marks of another upload
  ignored, a file by hand with its own fit, class and bytes), `lists`, `card` (the fold's notes)
  and `progress` (the rate). `scripts/drive/finder.json` with `scripts/drive/finder-fake.js`
  (and `machine-fake.js` for the machine line), run with `scripts/ui-drive.py … --init-script
  scripts/drive/finder-fake.js --init-script scripts/drive/machine-fake.js` against a dev instance:
  79 checks, every answer and write the fake's, nothing downloaded and no row created: the Models
  button, the empty finder, size marks then the header's, both measures and the switch, the
  windows and Show more, the file list's pick handed back, Get it's arguments, progress per file
  with the time left, setting up, done with the fit's note, "Say hello"'s thread and its address,
  Cancel, the metered state and Check exactly, NothingFits, the rate limit's wait and a page left
  during it leaving nothing behind (`same`), D9's question and its save, the hold, the disk
  short of room, the machine line, and the Overview's line. `scripts/drive/finder-live.json`
  searched "qwen3 0.6b" on the live hub (no token) and read its header (6.1 MB) on the dev
  instance's card, stopping before Get it.
  *Not yet:* part J's Chat host (§8.3); a choice of companions for
  Get it and Show more for other uploads (Not yet). *Unverified:* no real Get it ran (the drive's
  get, its frames and its status are the fake's), and "Say hello"'s `POST /chat/api/threads` was
  the fake's too, so a thread under a real public name and the Chat opening it on a model that
  isn't loaded are WP8's live round (*run: a real reply, and D110's fix*); the shots are headless Chrome at 125 %, not the app's
  WebKitGTK window, and in the dark theme only; `compact` has no host and wasn't looked at; the
  machine line was the machine fake's verdict, not a real one (*WP8: the real "ready", which
  shows no line*).
  **Review round (2026-11-04, D92, D93, D95, D97, D99 revised; D143 to D146).** *Built:* the
  lists' paging per list (`model_finder/paging.rs`: its own cursor, next page and offset, a later
  page's card leading it, nothing listed twice, D97), the lists' busy flag reset by a new search,
  the end's reads with none lost and a read again while the job isn't on the feed
  (`progress/track.rs`, D93), each file's line done, already on the disk, paused, failed or
  waiting by its row and the get's stage, keyed by file (D92), the progress's own `Scope` and
  Say hello's alive check (D146), the pick-only list with chat and aux only and no companions box,
  `model_id` none for a file from another repo (D99), the card's disk line left to the server
  after a stopped get (D144), one binary unit for every byte figure across the finder, the file
  list's tooltip, the sidebar, the Traffic page and the realtime budget panel, shared with
  `hf::fmt_bytes` (D143, binary for files too since the owner's ruling of 2026-11-04), Get it's file frozen (D145), the shared `HOLD_LINE`, the limit's live
  number and `HubBudget.allowance`, `ModelCard.likes`/`downloads`, plain sentences over raw
  errors, the accent bar. *Tests:* unit tests of `paging` (a window's offset on its page, page 2
  with its card and repeats, one list per request, the end, a card as an entry), `progress/track`
  (done only at the total, paused or failed by the get and the row, already on the disk, a read
  wanted mid-read kept, a running answer off the feed reading again), `progress` (the id only for
  the card's uploads), `words` (the GiB lines, D9's 24 as 24.0 GiB, the limit's notice and
  tooltip, a file's line), `wizard` (the pick-only classes) and `marks` (the tooltip in GiB).
  `finder.json` grew to 107 checks, 0 FAIL on a dev instance (prefix `lmgw-wp6`): Look further
  and Show more on page 2 per list, the licence as the hub names it, the pick-only classes, a
  file already on the disk, the bar's colour, paused and failed files, Cancel landing although
  its frame came during the status read (the fake answers a read as the get was when asked and
  sends the cancel's frame after its answer), the notice's number against the button's, the
  tooltip with the answer's allowance, D9's confirm (in GiB only since D143's revision), the disk line naming its
  button and gone after a stopped get. Shots 02, 06, 07, 10, 12, 14, 18 (a paused file) and 19
  (page 2) retaken at 125 %. *Not yet:* Cancel on a `joined` get (one that joined a get someone
  else started, `ModelGetStarted.joined`) cancels that get too, as D91's job cancel does; the
  owner ruled 2026-11-04 that it stays so (D91). A file whole on the disk before
  any get still gets the card's disk question once (D144). *Unverified:* the GiB figures were
  looked at in the fake's and the dev instance's numbers only, not on a real card's sidebar; the
  2-second re-read was driven by the fake's feed, not a real get's.
- **WP7 — Send to a friend (S; needs WP4, WP6; D100–D109).** The share text and its privacy test
  (§7), no link for a private repo (D124), the copy button, the Find box taking a pasted address
  end to end, and a README line under the quick start.
  **Built (2026-11-04, D100 to D107).** `model_finder/share.rs`: `share_text` and `share_for` (the
  display name, the upload and the shown file; `Text`, `Private` or `Nothing`, D101, D103), the
  `SendToFriend` button in the card's footer with its clipboard write and toast (D102), and
  `PrivateLine` with D124's sentence. `ModelCard.named_file` (api-types, set by `find::card`)
  and `shown::from_marks` keeping that file while its header fits it, so a pasted address is the
  friend's card after the marks too (D100). `words::paused_line` for the paused stage line
  (D106). A README sentence in the quick start (D107). The Find box needed no extraction of its
  own: `find::query` reads the address out of the pasted sentence (D104). *Tests:* unit tests of
  `share` (the exact text, the encoding as the server's, a card full of `*-demo` values leaving
  none in the text, a private repo, another upload by its own flag, an unlisted one, no file),
  `shown` (a named file kept while it fits, given up when it doesn't, and not kept on a card that
  was asked nothing), `words` (the paused line: one reason once, two kinds, none), `find::query`
  (the sentence without its line break); op test
  `a_pasted_share_text_plans_that_file_or_says_it_doesnt_fit` (the sentence with and without its
  line break, the bare address, one without its scheme, the share link naming the same file, and
  an 8 GiB card against the 9.5 GiB file: the card's note and the file that fits).
  `finder.json` grew to 125 checks, 0 FAIL on a dev instance (headless, scratch data dir): the
  share text pasted with Chrome's own newline stripping and the card on that file after the
  marks, the button between the two others, the clipboard's text exactly, the toast, the file
  sent after "Use this one instead", a private repo's sentence with no button and Get it kept,
  and the paused line as one sentence. `finder-fake.js` answers a pasted address and "demo
  private" and records the clipboard's writes. Shots 18, 20 and 21 were taken (not committed).
  *Not yet:* a select-the-text fallback where the page can't copy (D102); a link for a file
  chosen by hand in a repo the card doesn't list (D103). *Unverified:* the clipboard write was the
  fake's (`Clipboard.prototype.writeText`), so the real browser's permission and the WebKitGTK
  window's behaviour were not met (*WP8: headless Chrome's real clipboard refused the write
  for want of focus, and the toast said so, D111*); the share text was not pasted into a real second install;
  the friend's side was run against the mock hub and the fake, not the live hub (WP8).
  **Review round (2026-11-04, D100, D101, D102, D106 revised; D108, D109, D147).** *Built:*
  `waiting` typed on the wire with the reason decided server-side and the finder's stage line from
  it (D108), the waiting files matched by name rather than by a substring of the text; one
  `share_url`/`share_text`/`share_name` in api-types for the server and the dashboard,
  `ModelCard.origin_private` (D109); the other line labelled by its file's measure and the way
  back to a pasted file (D147); a pasted file kept unless its header says it fits nowhere (D100);
  the kit's clipboard with the refusal's reason in the toast (D102); the op doc and the MCP
  description of `model_get_status` name the reasons; the README says "the Find box" and that
  any Hugging Face link works. *Tests:* api-types `model_find::share` (the encoding with
  `Model[Q4].gguf`, `^ | ( ) ' " < > ` % { }` and a non-ASCII name, the text, a private
  original's name); core `find::card` (each of those names reads back through `find::query`
  bare, inside the sentence and without its line break), it `model_get::review` (the real
  metered note typed `metered` in `model_get_status`, and `wait_reason`'s three cases); `share`
  (every branch of `share_for` on a card and marks full of `*-demo` values, a token, odd numbers
  of VRAM, context, disk and download, a private other upload beside a public one, the whole
  card's file; a private original's name); `shown` (pasted = whole-card pick, = beside pick, =
  neither, both picks one file, a pasted file without a header fit or with marks by size);
  `words` (the typed paused line). `finder.json` at 138 checks, 0 FAIL on a dev instance (prefix
  `lmgw-wp7`): the paused line with no file name, no "Resume" and no doubled "Paused", the other
  line's labels and the way back for a pasted Q8_0 (the whole card's pick) and a pasted BF16
  (neither), a full disk's failed get; the fake's `waiting` and `share.text` in the server's
  shapes. Shots 18, 20, 22 (a full disk) and 23 (a pasted file that is neither pick) at 125 %.
  *Not yet:* the full disk's failed sentence (WP5's `failed_download` around `write_failed`)
  names the file twice and has no full stop. *Unverified:* a private original with a public
  upload was reasoned from the hub's `private` field and built from a unit test, not met on the
  hub.
- **WP8 — The live round (S; needs WP6, WP7; D110–D119).** On a dev instance with its own data
  dir and container prefix (never the installed app's), against the real hub: the 28 queries of §4.4 on a 24 GiB card, against the recorded table; one
  small model from "Find" through "Say hello" to a first reply; the app quit mid-download and
  started again; the hub plan against the disk plan of the same file after the download, which
  must be equal; the time estimate against the real download. The numbers go into this record's
  Unverified section as measured.
  **Run (2026-11-04, D110 to D114).** A debug headless instance on
  127.0.0.1:8118, a scratch data dir under `/tmp`, container prefix `lmgw-wp8` (read back from
  its settings before any start), no Hugging Face token, the local
  `ghcr.io/ggml-org/llama.cpp:server-cuda`, a 24 GiB test GPU with 0.7 GiB used by other programs and nothing of lmgw's on it. The numbers are in Unverified, "Measured in WP8's live round".
  *Found and fixed:* the Chat that "Say hello" opens called the new model "not served" (D110,
  `widgets/model_finder/progress.rs`); a refused copy's toast ended in two full stops (D111,
  `lmgw-ui-kit/src/clipboard.rs`, `copy_secret`). *Tests:* `finder.json` checks that the end
  reads the catalog again (`finder-fake.js` counts `/v1/models`); `clipboard::tests` holds
  Chrome's refusal in one sentence. The live drives themselves (drive files and shots, not committed) are throwaway: they download real files. *Found, not
  fixed* (D114, Not yet): "qwen3 1.7b" answers another kind (since fixed by ruling 3, D15,
  D148); a split set's input embeddings are charged to the card. `finder.json` on an instance that already has local models fails
  its four Overview checks (the empty-state line shows only without models): it wants a fresh
  scratch dir, as WP6 ran it. *Unverified:* the WebKitGTK window (headless Chrome only, dark
  theme, 125 %); a real clipboard write (headless Chrome refused it for want of focus); a
  metered connection; a crash (SIGKILL) mid-download (D113); a model larger than 1.8 GB, and
  any start of the files the header fit marked near the card's edge (one model on the GPU,
  the 0.6B).

## Not yet

- Part C's purpose entry points ("something for coding", "something small and fast"). The hub's
  live `pipeline_tag`, the `conversational` tag and the header's facts could answer some of them
  without a list, and K's fit and pick could serve them. They are not designed.
- An `lmgw:` link that opens lmgw at the finder (D32). It needs a scheme handler in the RPM's
  desktop file and a single-instance hand-off in the shell. The pasted address works without it.
- Fit on the CPU: a row with `n_gpu_layers 0` measured against the computer's memory, for a
  computer with no usable graphics card. The card says "no graphics card lmgw can use" for now.
- MoE expert offload to the CPU in the fit: a mixture-of-experts model whose experts could live in
  the computer's memory while the rest is on the card (llama.cpp's `--n-cpu-moe`), such as
  Qwen3.6-35B-A3B on an 8 GB card, which §4.4 answers `NothingFits` today. Since 2026-11-09
  admission and the footprint count a row's `n_cpu_moe` field, `--n-cpu-moe`, `--cpu-moe` and
  `--override-tensor` patterns to the CPU, matched against every part's tensors, with the
  op-offload copy in the compute buffer and the `moe_cache_mib` / `--moe-cache-mib` expert cache (decision-models design, Appendix C.2, "Tensors kept in
  host memory"). No plan path sets `n_cpu_moe` yet, so choosing N is not yet done anywhere: the
  plan, `hf_repo fit` and `model_find` don't pick an `n_cpu_moe` for a model that doesn't fit,
  and their fit counts every weight on the card for now.
- Taking an untied output head from a split file's later part (D3): WP2 reads those headers for
  the count and the width (D54), and the output head is still `token_embd.weight`'s; the plan's
  footprint, as admission's, still charges `token_embd.weight` to the card when the untied head is
  in a later part. The file list's band counts every part's input tensors since WP3's fix round
  (D61). *Measured in WP8 (D114):* `ModelSummary::host_input_bytes` is part 1's (`gguf.rs`,
  `output.weight` beside `token_embd.weight` in one header), and on
  `Qwen/Qwen2.5-Coder-32B-Instruct-GGUF` the split `q4_0` fits 29 952 tokens where the single
  `q4_0` fits 33 024, `q4_k_m` 21 248 against 24 320, and the split `q5_k_m` doesn't fit where
  the single one fits 768: 3 072 tokens less each time, about 0.8 GB of its cache (64 layers ×
  8 KV heads × 128 × 2 × 2 bytes a token). Read by Range (a spike script, not committed): the
  split `q4_0`'s part 1 holds `token_embd.weight` (Q4_0, 5 120 × 152 064, about 438 MB) and
  part 3 `output.weight` (Q6_K), the single file both. The rule charging the former to the
  card explains about half of the 0.8 GB; the rest wasn't traced.
- "by <company>" for an organisation whose display name isn't the company's (D19).
- A description of what a model is good at. Nothing live states it in a form lmgw can rely on
  (D22).
- Telling a specialist model from a chat model when the hub's tags and pipeline don't (the
  "granite" miss of §4.4).
- Part J's Chat slot for the finder (§8.3), after WP3b. `ModelFinder` takes `compact` for it, and
  no host sets it yet.
- A choice of companions for the finder's Get it: `model_get` takes none, so Get it fetches the
  projector and the drafter, and the pick-only file list shows no companions box (D99).
- "Show more" for the card's other uploads: the fold lists the answer's window (10) and how many
  more there are (D142); the lists of other models and partial matches have it (D97).
- Engine support for the architecture before the download: a file whose architecture the
  engine's llama.cpp doesn't know yet is found at the first message, as today
  (`load_failure_hint`, "update the container image").
- Picking a revision other than `main` in the finder: the file list takes a repo at `main` too.
  `hf::remote_header` reads at `main` only (D43).
- An image plan's fit on the card: `local_model_plan` with `target=image` answers no `source` or
  `fit` yet (D137); an image pipeline's footprint is the image class's own estimate, which the
  fit doesn't call yet.
- The projector rule for drafters: `hf_add` queues every drafter beside the weights and the disk
  plan takes the first its scan lists (D41).
- Checking a file downloaded before WP1's review round against its sha256: only a transfer
  checks the bytes it writes (D130), and nothing reads a finished file again.
- A `paused` status of its own on `hf_models`: the metered pause is a queued row with its
  sentence (D47), and `lmgw__hf_downloads` shows it as `queued` with that `error`.
- Choosing between two kinds Find a model sets up that both match every word *(WP8, D114)*:
  likes decide between them, so "qwen3 0.6b" gets the chat card because `Qwen/Qwen3-0.6B` has
  more likes than `Qwen/Qwen3-Embedding-0.6B`. A rule for it (fewer words beyond the query's,
  say) isn't designed; ruling 3 settled only a kind Find a model sets up against another kind.
- The download time on a fresh install's first card *(WP8)*: `model_find` reads no header, so
  before any download finished the card says "the time shows once lmgw has measured a
  download", and the header reads the page makes right after (D23's fallback rate) don't
  change that line.

## Findings kept for later

The hub can answer purpose questions without a list: `pipeline_tag` (`feature-extraction`,
`image-text-to-text`, `text-generation`) and the `conversational` tag were present on many
originals of §4.4, though not on all of them (D15), and `expand[]=gguf` carries the architecture,
the trained context and the parameter count of an upload. With `expand[]=gguf` the answer is
7.6 KB a repo, mostly the chat template. A page of 1000 results was accepted (113 KB without
expansions). The listing's `safetensors.total` is free with the search and was enough for every
pass-over of §4.4.

## Unverified (2026-11-03)

- WP1 to WP7 are built and WP8's live round ran (2026-11-04, its block at the end of this
  section). WP5 ran against mocks only (its entry's Unverified). WP2's hub plans ran against a mock
  CDN serving synthetic headers; WP3's marks then ran against the real hub through a dev
  instance (no token): every weights file of `unsloth/Qwen3-32B-GGUF` and
  `unsloth/Qwen3-8B-GGUF` was marked on a 24 GiB test GPU, the files read exactly went through
  lmgw's own planner from their real headers (`Qwen3-32B-Q4_1.gguf` read 5.0 bits per weight,
  `Qwen3-32B-IQ4_XS.gguf` 4.26), and the pick, its chip and the line were seen in the window
  (shots, not committed). The footprints were not compared with
  a real start of any of those files (not run in this round), so whether a "fits" holds at the first
  message is still unchecked. The header sizes and times of §2.2's table were read by a Python
  reader that walks the same format.
- The review round's checks ran against mocks only: the sha256 of a real multi-gigabyte download
  was not compared by hand (WP8 compared a resumed 1.8 GB one, below), `nmcli monitor`'s lines on a real connection switch (and whether it
  reports a connection's metered setting being changed in place) were not seen, and a `416` from
  the real CDN over a whole `.part` was not tried. The prefix read's speed over a 16 GB `.part` was
  not measured.
- WP1's resume was run against the mock CDN only. Against the real one, a by-hand Range from a
  mid-file offset (byte 500 000 000 of `mmproj-F16.gguf`) and an open-ended one near its end
  answered `206` with the right `Content-Range`, read with curl. WP8 cut and continued one real
  download of a xet-backed weights file at a mid-file offset (below). A real
  `nmcli` saying "yes" was not seen: the metered pause ran on a stated machine.
- The Downloads page's "paused: metered connection" chip was not looked at in a window: it is a
  branch of the existing "interrupted" chip, built and type-checked only.
- F1 was read in code (`vram/plan.rs:223`, `plan_fit.rs:111`), not run against a split row. Its
  fix is fixed in a separate commit.
- That the conservative estimate of §3.3 is never smaller than the exact one is an argument from
  the footprint's terms (only the host-kept input embeddings differ by quant, apart from the file
  size). WP2's test holds it over synthetic headers of three widths with untied embeddings, not
  over recorded real headers (11 to 16 MB each, not committed).
- `hf::rate::engine` (Podman's presence, the registry manifest's layer sizes and the image
  store's layers, D23, D58) ran in WP8 only on its "here" branch (the image on the machine);
  the registry's and the image store's branches were type-checked, not run.
- D140's ggml type sizes were checked against two real files only (F32, Q8_0, Q4_K, Q6_K in
  `bge-small-en-v1.5-q8_0.gguf` and `bge-reranker-v2-m3-Q4_K_M.gguf`); the other types are
  `gguf-py`'s `GGML_QUANT_SIZES` as copied, and no real MoE or Mamba file's F32 2-D tensors were
  seen.
- The rate-limit figures are what the hub's headers said to an anonymous client on 2026-11-03
  (`"api";q=500;w=300`, `"resolvers";q=3000;w=300`). The "by …" call was seen counting against
  `api`. *(WP8, 2026-11-04:)* the tree counts against `api` too: over 56 queries the
  answers' `RateLimit` `r` fell by exactly the calls the walk counted (500 to 233 for 267), and
  the policy was the same.
- §4.4's spike picks files by size from the listing's parameter count, without D7's header count
  or a header fit. Its cards are what WP4's gate holds; its files are not.
- The private-repo case (D124) was reasoned from the hub's `private` field, which the anonymous
  listing accepted; no token was sent, so no private repo was seen.
- Whether every xet-backed file answers a Range read from a mid-file offset (D122) as it answers
  one from byte 0 (all six tried did), and whether older LFS-only files redirect to a different
  CDN host.
- The slow-line times of §2.2 are 11 MB divided by the line's rate, not measured on such a line.

**Measured in WP8's live round (2026-11-04, a dev instance on a 24 GiB test GPU, no token; D110 to D114).**

| What | Measured |
|---|---|
| §4.4 on 24 GiB (a test GPU, 0.7 GiB used by other programs) | 28 of 28 cards as the table and the regression set have them (the original, and the upload where one is named; granite's miss as recorded); 137 hub calls (3 to 6 a query), 30.5 s wall (0.4 to 1.4 s a query); answers 3.4 to 9.0 KB |
| §4.4 on 8 GB (a declared 8 GiB budget, D112) | 28 of 28; 130 calls (3 to 7), 27.7 s wall; 267 of the anonymous 500 for both, in one window, so no wait |
| Hub plan = disk plan | Equal in every field but `source` for `Qwen3-0.6B-Q8_0.gguf` (its header from the cache, 0 bytes) and the resumed `Qwen3-1.7B-Q8_0.gguf` (6 105 677 header bytes read) |
| Quit mid-download (D113) | Get it on `Qwen/Qwen3-1.7B-GGUF` `Q8_0` (1 834 426 016 B); SIGTERM at 8.06 s with 40 % in, the instance gone 0.9 s later and the 0.6B's container stopped; 837 784 369 B kept in the `.part`; on the restart the download went on "from byte 837784369" (logged) and finished 10.15 s later, the get re-attached ("1 get(s) the last shutdown cut go on"), the row created at 40 960 tokens and nothing started (D29). The file is xet-backed (`x-xet-hash`), and its sha256 by hand is the hub's `oid` (`061b54da…`) |
| Time estimate vs download | First card on the fresh install: "the time shows once lmgw has measured a download" (Not yet). After the 0.6B (639 MB), the 1.7B's card said "less than a minute" (18 s on the test connection); the real transfer took 8.1 s + 10.2 s = 18.3 s, the second half including D130's sha256 pass over the kept 838 MB |
| Find to a first reply (headless Chrome, 125 %, shots 01 to 13, not committed) | "qwen3 0.6b" → Qwen3 0.6B, `Qwen3-0.6B-Q8_0.gguf` from `Qwen/Qwen3-0.6B-GGUF`, "fits", 40 960 tokens, "Uses 4.1 GiB of the 21.8 GiB free … beside what runs now"; Get it to done in about 7 s; Say hello opened `/chat?t=1` under `qwen3-0.6b`; the first message loaded it in 1.4 s, ttft 1.7 s, 502 tokens/s, a real reply. The picker's "not served" there was D110's bug, fixed and driven again (shot 12) |
| The card's footprint vs the card | The 0.6B at 40 960 tokens: plan 3.14 GiB (weights 0.64 GB, cache 2.50 GB, compute 0.23 GB), the card's line 4.1 GiB with the headroom; llama-server's process held 3.56 GiB (`nvidia-smi`), 0.42 GiB over the plan and inside the 1 GiB headroom |
| Fit again on the created row | "The model already takes the largest conversation that fits …: 40 960 tokens", no change, no note (its context is the trained one) |
| Header fits of the files §4.4 flagged | phi 4 on 8 GB: by size `Q3_K`; its header reads 3.94 bits a weight, under the floor, and no file of 4 to 8.5 bits fits 8 GiB with 16 384 tokens (pick: none), as §4.4 foresaw. bge m3: `bge-m3-q8_0.gguf` (8.5) over the f32 by size. embeddinggemma on 8 GB: `Q8_0` over the BF16, 35 566 of 262 144 tokens. gemma 3 on 8 GB: by size `UD-Q8_K_XL`, its header 9.51 bits (over the cap): `UD-Q6_K_XL` on the whole card (38 656 tokens), `Q6_K` beside. qwen on 24 GB: by size `UD-Q6_K`, its header doesn't fit at any context: `UD-Q4_K_XL` (43 520) and `UD-Q4_K_M` beside; 167 MB of headers in 14.6 s. qwen 2.5 coder on 24 GB: by size the split `q4_k_m`, by header the single `q4_0` (33 024), none beside at 32 768; 61 MB in 23.9 s |
| The machine check, the engine | The real verdict "ready" (no machine line on the card, D11); `hf::rate::engine` said "here" for the local image through Podman |
| The hub's gzip (D141) | `/api/models` for "mistral small" with the expansions: 599 972 B, 126 686 B gzipped (21 %) |
| The share button, headless | Chrome refused the write ("Document is not focused"); the toast said so (D111) |

Differences from §4.4's table, none in a card: the likes moved by a few (mistral small's
1409 to 1408: the hub); "deepseek v4 flash" names the base V4-Flash, "nemotron" on 8 GB
Nemotron-Orchestrator-8B, and the `NothingFits` cards on 8 GB name the original closest to
fitting (qwen 3.6: Qwen3.6-27B, mistral small: Mistral-Small-Instruct-2409, gpt oss: gpt-oss-20b,
kimi: Kimi-VL-A3B-Thinking): the rules since WP4 (D17, D70), as the regression test has them.
The calls are fewer (137 against the spike's 156). The files by size differ from the spike's
at the card's edge (qwen 3.6 `UD-Q4_K_M` for `UD-Q4_K_XL`, gemma 3 `Q6_K` for `UD-Q6_K_XL`):
the test card reports 22.5 GiB, where the spike's and the regression set's
"24 GB" is 24 GiB, so the largest file the room holds with the headroom is the next one down;
the header fit decides the file after (row above). Not seen
live: "qwen3 1.7b" is a query outside the set that answered another kind (D114; the kind first
since ruling 3, D15, D148).

## Review round (2026-11-03)

Each finding was checked against the code at 94c735b2 or the live hub before it was taken in.

- **B1, ranking:** confirmed live (the first draft's D15 put a draft model, a Qwen finetune and an
  uncensored upload first). D13, D14, D15 and D17 are revised, D126 is new, and the 28-query
  regression set of §4.4 is WP4's entry gate.
- **H1, projectors:** confirmed (`ops/huggingface.rs:384–410`, `modelinfo.rs:1388–1404`). One
  correction to the finding: the three projectors of `unsloth/Qwen3.6-27B-GGUF` are 3.70 GB
  together, so a default `hf_add` fetches 2.77 GB more than the one it needs, not 3.7 GB more.
  D121, fixed in WP1.
- **H2, restarts:** confirmed (`store/jobs_table.rs:179`, `state.rs:591`,
  `jobs/hf_download.rs:179`). D122, in WP1 and WP5; the Range resume is in, not deferred.
- **H3, F1:** recorded as fixed separately; WP2 depends on it.
- **M1, the whole card:** D5 revised, D120 new (the beside pick, shown by default).
- **M2, the parameter count and the call count:** the "by …" call was uncounted (it counts
  against `api`, measured). `expand[]=safetensors` is not a call of its own: it rides the search
  (6 % more bytes, measured), so that part of the finding was wrong, and the listing's total stays
  for the pass-over. D7's count comes from the header, and §4.4 recounts.
- **M3, header bytes:** a drafter's header measured (7.9 MB, 1.7 % of its file). D123 new; D1,
  §2.1 and §2.2 revised.
- **M4, privacy:** D124 new; D31 and §7 revised.
- **M5, answers:** `NoRunnableFile` confirmed live ("gemma four"), matched-word ranking (D13), the
  `NoMatch` sentence (§4.5).
- **M6, aux context:** confirmed (`aux_plan.rs:394–404`); D26 and §6.2 move the tied
  batch with the context.
- **M7, the file list:** D34 revised to a pick-only mode.
- **M8, the WPs:** `FileFit` and D23 in WP2; WP3 to WP5 land in sequence; WP2 is L and starts
  with the disk reads (`plan_fit.rs:86`, `:101`, `aux_plan.rs:383`, confirmed); the missing tests
  are in §10.
- **M9, the API:** confirmed (`web/api.rs:399–403` shares `InspectQuery`); §9.3 and D36 revised,
  `fit` false by default.
- **Lows:** WP1's `get_with_commit` change (§1, §2.1); D4 on `lfs.oid`; D27's route and public
  name; D28 across `/v1`; D23 by bytes with the engine; D125; D9 inline; D14's fold; MoE offload
  in Not yet; `RateLimit-Policy` and "requests" (§4.5); D10's resume line; D22's licence link.
- **D7, D18, D26:** put to the owner and ruled on 2026-11-04: D7 and D26 as the review proposed
  them, D18 as first drafted (no number-word retry).
