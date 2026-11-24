# UI rebuild — Leptos replacement for the htmx dashboard

Status: **done — P0–P8 built and verified** (2026-08-29). The Leptos SPA owns
`/`; the askama/htmx UI is deleted. Companion docs: [parity.md](parity.md) — as-is
inventory of the old UI (kept as the record of what was replaced);
[core-notes.md](core-notes.md) — core internals brief. This doc is the target
architecture and the phase plan.

## Goals (2026-08-29)

- Fully new UI, **feature complete** vs the old one, but free to reorder / merge / split
  pages. Modern, efficient, **quick, live and stateful** — no infinite scrolling, real
  visual structure, desktop-app feel (it always runs as a Tauri desktop tool).
- **Hybrid design, one system**: clean / minimal / round foundation (Claude / Gemini /
  ChatGPT register) for user-facing surfaces (chat, audio), a **dense variant of the same
  components** for admin surfaces. Same tokens, radii, palette; different spacing and
  information density. One user (the owner), daily driver, UI and core ship together.
- **HF-first model management**: adding a model starts from a Hugging Face repo (search,
  pick quant, download). Local GGUF file selection stays available only as the exception
  path. Wizard-shaped flows where multi-step.
- Automation (explicitly chosen): **auto-plan + auto-create when an HF download
  finishes**, and **HF update tracking** (detect new revisions of downloaded files,
  one-click re-download). Explicitly **not** chosen: auto-apply on save and auto-test
  after apply — container apply stays a deliberate, visible step.

## Architecture decisions

- **Leptos CSR** (client-side WASM), not SSR/hydration. Streaming needs (live logs, live
  generation, download progress) are served equally well by SSE in both modes; SSR's
  first-paint advantage is irrelevant for an always-on local tool. CSR keeps
  `lmgw-core` free of Leptos/WASM build coupling.
- New workspace crate **`crates/lmgw-ui`**: Leptos + leptos_router, built with **Trunk**
  to `dist/`, served by `lmgw-core` as embedded static assets (rust-embed style: embedded
  in release, read-from-disk in debug for a fast dev loop with `trunk watch`).
- New workspace crate **`crates/lmgw-api-types`**: serde DTOs shared by the JSON API
  (server side) and the UI (client side) — type-safe end to end, no hand-kept parallel
  types.
- **JSON + SSE admin API under `/api/`** in `lmgw-core`, alongside (not replacing) the
  existing htmx routes. Strictly separate from the `/v1` LLM plane. Live views (log tail,
  download progress, container state, generation preview) are SSE endpoints designed
  first-class, not retrofitted.
- **Parallel mount during the rebuild**: new UI at **`/ui`**, old UI stays at `/` until
  parity. At the end (done in P8): the SPA moved to `/`, `/ui/*` became a permanent
  redirect to the same path at the root, and the askama templates, htmx assets and old
  web handlers were deleted. The Tauri window already pointed at `/`, so nothing to flip
  there. The SPA's MCP page moved `/mcp` → `/mcp-servers`, because `/mcp` at the root is
  the northbound MCP protocol endpoint.
- Old UI receives **no changes** during the rebuild (frozen except critical fixes).

## Information architecture (draft — refine against parity.md)

Left **sidebar** (grouped), replacing the top tab row; content area uses master-detail
layouts instead of separate edit pages where it fits.

- **Use** (minimal density): Chat · Audio Lab · Workflows
- **Serve** (dense): **Models** — one unified surface for model aliases, local chat
  models, embeddings, audio models; origin (cloud upstream / local llama.cpp) and type as
  facets, not separate tabs. Primary action: **Add from Hugging Face** (wizard: repo →
  quant → download → auto-plan → created). Downloads + HF update tracking live here.
  Explicit **Apply** bar whenever container config is dirty.
- **Connect** (dense): Upstreams · MCP servers
- **Observe** (dense): Dashboard/Status · Traffic (logs + responses merged: live tail →
  detail drill-in) · Wiring
- **Settings** (dense)

Inventory-driven constraints (details + line refs in parity.md):
- **Three managed containers** — chat router, embed router, audio.cpp. Apply is a
  separate deliberate step *per target*; the apply bar and downloads views are
  per-target, not global.
- **Custom dropdown component required** everywhere: WebKitGTK ignores CSS on native
  `<select>` popups (that's why select.js exists). Same for consistent modals.
- **Secrets convention**: "empty input keeps stored value" + explicit clear checkbox.
- **Chat/Audio Lab** are already Lit+JSON/SSE mini-apps; their JSON APIs (threads, tasks,
  voices) are the porting base. Chat streams SSE over POST bodies → hand-rolled SSE
  parse over fetch ReadableStream in WASM too.
- **Download tables poll only while something is active** — keep that; better: SSE.
- Dashboard backend links must follow the request `Host` header (LAN access).
- Local-model path fields: container path prefix stripped on write, shown on read.

## Modern design (2026-11-15, contract: modern-sample.html)

The dashboard moved to a libadwaita/macOS design language, approved from the
mockup `modern-sample.html`. WP1 laid the foundation every page uses; the pages
are ported one by one after it. What changed against the 2026-08-29 system below:

- **Type**: Adwaita Sans (UI, a variable font) and Adwaita Mono (code, data),
  bundled as woff2 from GNOME's adwaita-fonts 51 with their OFL text
  (`crates/lmgw-ui/assets/fonts`). The window no longer switches to the desktop's
  own fonts: the window, a browser and the Windows and macOS apps set the same text.
- **Tokens** (`crates/lmgw-ui-kit/assets/kit.css`): the mockup's neutrals, in the
  kit's names (`--bg0` sidebar, `--bg1` view, `--pane` page, `--surface` card,
  `--surface-hi` raised; `--popover`), plus its translucent roles (`--hover`,
  `--select`, `--divider`, `--card-line`, `--ctl`, `--ctl-hover`, `--field`,
  `--focus`), radii (`--r-item` 8, `--r-box` 12, `--r-win` 14) and `--head-h` 48.
  Dense controls are 32px, table rows 36px. Eyebrows are sentence case, not
  uppercase tracking. The desktop palette (src-tauri `desktop_palette`) steps the
  same tokens off the desktop's title-bar shade.
- **Shell**: a full-height sidebar in its own tone with its own header bar (the
  toggle to the icon rail, the app's name, the main menu: theme, interface scale,
  Settings) and the status foot (the GPU pill reads "On hold" while the hold is on); no line between sidebar and content. Every
  content route has a 48px header bar (`PageFrame`, or `HeadBar` for the Chat,
  the labs and the sign-in card): title and subtitle, tabs, the page's actions,
  then, with the shell's own frame (the default on Linux; `LMGW_WINDOW=kwin` gives
  the desktop's title bar instead), the window buttons in the
  desktop's order. Headers are `data-window-header`; the page rounds its own
  corners by `--csd-radius` (`crates/lmgw-ui/src/frame.rs`).
- **Components**: one set, restyled in place (buttons default/flat/suggested/
  destructive, icon buttons, fields, dropdowns, checkboxes, toggle groups, chips,
  tables, cards, dialogs, popovers and menus, toasts as dark pills). New in the
  kit (`widgets/prefs.rs`, `widgets/icon.rs`): `PrefGroup` with `SwitchRow`,
  `SpinRow`, `ComboRow`, `ActionRow` (danger), `InfoRow`; `Switch`,
  `SpinButton`; `ListRow` with `StateDot`; `SaveToast`; `Icon`; and the
  `.detail-flow` two-column container query. A debug build shows them all at
  `/dev/components`. A record's state is one `Dot` for its list row and its hero
  chip (`StateChip`, `Dot::chip_class`): resting states are neutral, work in
  progress the accent, needs-a-look amber, failed or expired red, ready after a
  check green. A clip plays in the kit's `AudioPlayer` row, not the browser's
  controls (the API reference keeps its native player for now); a source of
  `None` is no clip yet, a clip that fails to load says so on the button. A menu
  button is a `RowMenu` with a `label`, an optional leading `icon` and a
  `chevron`. A row that cannot act now is an `ActionRow` with `disabled`. Text
  areas are in the UI font; code, JSON and arguments one per line opt in to the
  monospace face (`mono`). Links have no
  underline at rest and gain one on hover; prose (`.md`) keeps its underline.
  Cards, notices and the machine check's callouts are boxes without a border
  (the `.boxed` edge: a hairline ring and the light theme's shadow); a warning or
  error notice is the same neutral box with an amber or red icon leading it, in
  both themes, never a tint. A table card is edge to edge. A table row's buttons
  rest in the secondary colour and come to full colour under the pointer or the
  focus. A detail's header bar without tabs shows the record's name once its
  hero has scrolled away. Signed out, the sidebar's pages are dimmed and inert.
- **List pane**: every list + detail page builds its list pane from one shared
  module, `crates/lmgw-ui/src/widgets/list_pane.rs` (the header bar, the search,
  the keys and cursor, the scroll to the selected row, the kept list query and
  the guarded navigation); rows, grouping, filters and add menus stay the page's.
- **Pages** keep their layouts until their own work package. The planned order:
  1 Models (the mockup: a list pane with `ListRow` beside a `PrefGroup` detail
  with `SaveToast`), 2 the local model editor (becomes the Models detail),
  3 Settings, 4 Chat, 5 the labs (Audio, Image,
  Decision, Voices), 6 Agents and the agent detail, 7 Upstreams and MCP
  servers, 8 Engines, Downloads, the model catalog and Find a model, 9 Docs and
  Knowledge, 10 Traffic, Conversations, Benchmarks, Wiring and the API
  reference, 11 Usage and Overview (the closest to the new design already).
- **Models** (1 and 2 above, 2026-11-15): the list beside the detail
  (`pages/models/split.rs`, libadwaita's navigation split view). The list has
  the class and state filter under its title (`?show=`), the search (`/`,
  Ctrl+F, Esc), the add menu and Downloads, and the rows by class under sticky
  headings with live state dots; the arrow keys move the selection. The
  selection is the path on one route, `models/:class?/:id?`
  (`/models/local/7`, `/models/aux/2`, `/models/local/new`,
  `/models/upstream/<id>`), so links from other pages select their row and a
  move away from unsaved edits asks. The list's divider resizes it (kept as
  `lmgw.ui.models.list.width`). Under 800 px of pane (a 1024 px window at
  125 %) one pane shows at a time, the detail with a back button. The local
  chat editor is ported whole into preference rows
  (`pages/models/detail/local/`), with the save toast and Ctrl+S. The aux,
  audio and image editors, the cloud aliases and the candidate aliases keep
  their forms in the detail pane: their port to rows is not done yet, and new
  rows of those classes are still made in their modal editors. The hero's
  running chip has no tokens per second yet: the event stream carries the
  gateway's rate only. `scripts/drive/models-split.json` drives the view (it
  saves: a dev copy only).
- **Settings** (3 above, 2026-11-15): the categories beside the shown one on
  the same split view as Models, now shared (`crate::widgets::nav_split`:
  the measured pane, the remembered divider `lmgw.ui.settings.list.width`,
  one pane at a time under 800 px with a back button, Ctrl+F and Ctrl+S).
  The category list has the title with the version and data directory, the
  search (`?q=`, `/`, Ctrl+F, Esc) and one row per category with its glyph,
  which says how many of its settings are unsaved, or during a search how
  many match; ↑ ↓ Home End move the selection. A category is a hero and its
  groups as boxed preference groups (two columns over 1000 px; the
  groups marked wide in the table span both and come last); a runtime
  class's group folds from its heading, explanations fold under the
  heading. Each setting is a row (`pages/settings/prow.rs`): numbers,
  short text, switches and choices at the row's end, paths, lists, prompts
  and pickers under the title, links with a chevron, the actions that apply
  at once (hold, Apply, Check now, Forget learned growth) with a button and
  an "applies now" tag. One draft for the page as before: the save toast
  (change dots, Ctrl+S, a field to fix blocks Save) replaces the save bar, a
  category switch keeps the draft without asking, leaving Settings asks. A
  search shows every hit under its category, editable; in a narrow window
  the list shows the hits as links instead. Not ported to rows yet: the
  read panels keep their own inner layout as one row of their group (the
  machine check's card, the TLS listener's state, the accounts and their
  sessions, the audit log, the device listener, the WebRTC panel, the
  cascade's GPU memory, About, the Smart Turn table). `scripts/drive/
  settings-split.json` drives the view (it saves: a dev copy only).
- **Chat** (4 above, 2026-11-15): restyled in place in `lmgw-chat-ui`, its
  layout mechanics kept (the kit's `SplitPane` with its rails, the look over
  the conversation and the narrow layout's drawers; the shared nav_split
  does not fit three panes and the drawers yet). The thread list is a pane
  in the sidebar's tone whose head is a header bar: the title, New chat as
  a compose button, a menu with a new Admin chat, temporary chat and folder
  and the export of every chat, the fold button; under it the search and an
  Active and Archived switch with their counts, the rows as the kit's list
  rows under sticky date headings, ↑ ↓ Home End moving the focus (Enter
  opens). The conversation has its own 48px header bar spanning it and the
  thread settings under it: the thread's title with its model as the picker
  under it, the thread's marks as pills, the download menu and the window
  buttons, which the host hands in (`ChatHost::window`; the list's head
  takes the start ones while it is open beside the conversation). The
  transcript is a 760px column on the view: the user's turns as soft
  bubbles, the assistant's flat, reasoning, tool calls, approvals, task
  results and the machine check's card as boxed cards, code blocks with a
  quiet header band (language, Preview, Copy), boxed tables; the composer
  a rounded field with flat icon buttons, Send the round accent button.
  A reply ends in one row: its action buttons (shown on hover) and, at the
  row's right end, a dim "prompt → completion tokens" with the answering
  model only when it is not the thread's own or a fallback answered. The
  thread's MCP mark is the plug icon with a count. Read aloud under the GPU
  hold takes the quiet resting look, not amber, and stays pressable.
  The strips above it are rounded notes, the voice panel a rounded card in
  the same tones. No colour is the Chat's own: a host's palette restyles it.
  Not done yet: the thread settings are still the form they were (not
  preference rows), the folder dialogs keep their forms, the thread list
  has no header toggle in place of its rail, the media cards keep their
  layout, and a head with every mark on and a long title wraps its marks
  onto a second line instead of folding them into a menu.
- **Chat profiles** (2026-11-22): `/chat/profiles/:sel?` is a list beside
  the selected profile's detail on the shared split view, like Upstreams.
  The selection is the path (`/chat/profiles/<id>`, `/chat/profiles/new`),
  `?thread=` and the search (`?q=`) are carried across every selection. The
  list head has the back button to the Chat (or to the thread) at its start
  and a + menu (New profile, Add the built-in Concise); rows show the name,
  "Default" on the profile new threads start with, and who uses it. Nothing
  selected is a status page (the count, the default as a link to Settings,
  Add, and what `?thread=` makes Preview, Test and Speak do). A profile is
  the kit's `ProfileEditor` in preference groups (Profile, Text, Examples,
  Voice block, Behaviour, Static part, Test, Speak a sample, then Reset and
  Delete as rows; Delete asks in a dialog what it clears), under a hero with
  the built-in, usage and default chips; changed rows carry the accent dot,
  the save toast counts them, Ctrl+S saves, and leaving with edits asks
  through the shared guard. The kit's `ProfilesPanel` stays for the desktop
  client. Not done yet: the Static part, Test and Speak groups are always
  open (no fold), a reset toasts as a save, a failed delete says "Not
  saved", and the editor is one column at every width.
- **Labs** (5 above, 2026-11-15): the Audio, Image, Decision and Voices
  labs restyled on their shared frame (`pages/lab_frame.rs`), their logic
  kept. The header bar is the lab's name with its model picker as the
  subtitle (as the Chat's), the task menu and the model's family or facts
  as pills after it (the Decision lab's facts fold into a Facts popover
  below 1200 px of pane), and the actions at its end as flat icon buttons: the
  model's state, the presets (a bookmark button opening a popover with each
  saved preset's Load, Update and Delete and the field that saves the form;
  `pages/lab_presets.rs`, no longer a card in the form) and Reload. The
  form column is a preferences pane on the page's tone: the parameters are
  boxed groups of the kit's rows (entries, dropdowns, switches, a JSON box,
  a file picked from a row's button), each naming the request field it sets
  after its title in a quiet monospace (`wire`, `WireName`), the prompt a
  rounded field under its heading (`pages/lab_rows.rs`), the Decision
  lab's cards boxed groups under their headings. The run bar ends in a pill
  run button. The results column is in the view's tone: its head the count
  over the kept size, the model filter, Save or Export as an icon menu and
  Clear; the runs cards without outlines, a failed one edged red, their
  JSON wrapping instead of scrolling sideways. The reference panels keep
  their facts in boxes, the voice library's clips and samples as rows with
  Play, ASR and Delete (a clip's name has the first line, its size and
  buttons the second), the question sets and set runs as list rows with
  the loaded set's name as a dim subtitle beside the "Question set" heading.
  An empty results column is a status page: the lab's glyph tile, "No runs
  yet" and the lab's own sentence. In the one-column view the Form | Results
  switch names the view, so the results head drops its own "Results".
  The labs play audio in the kit's `AudioPlayer`. Labels that were config keys read as words.
  The history stays a column of cards beside the form rather than a list
  pane beside one opened run: the cards play, show and compare their runs
  in place, which a list row cannot. `scripts/drive/lab-fake.js` fakes
  populated Image, Decision and Voices labs in the page, and
  `scripts/drive/labs-modern.json` drives the new markup on it (no model,
  no GPU). Not done yet: the entries are text rows, not spin buttons (a
  blank still means the server's default); the Decision lab's question
  editor, images, compare and token cards keep their inner controls inside
  their boxes, and its set run page (`/decision-lab/batches/<id>`), the New
  set run dialog and the Voices lab's candidate and save rows keep their
  layouts; the Audio lab's and the Voices lab's header pills still wrap to
  a second line in a narrow pane (the Decision lab's facts fold into a
  Facts button below 1200 px of pane); the Audio catalog and
  the Image recipes, which open from Models' add menu, are not part of this
  port.
- **Agents** (6 above, 2026-11-15): the catalog is a list beside the
  selected agent on the shared split view (`pages/agents.rs`,
  `pages/agents/`), one route for both, `agents/:id?/:tab?`, so
  `/agents/<id>`, `/agents/<id>/runs`, `/agents/<id>/ui`,
  `/agents/<id>/definition` and the old `?tab=` links select their row
  (the divider kept as `lmgw.ui.agents.list.width`, one pane at a time
  under 800 px with a back button). The list has the title with how many
  agents it shows, the add menu (New agent, Import a manifest, Install
  from an image, Restore shipped agents), the search (`?q=`, `/`, Ctrl+F,
  Esc) and a row per agent under its run kind's heading: the kind's icon,
  the name, its state (ready, running from the jobs feed, disabled, tools
  missing, blocked, warnings) and a line of facts, its warnings in the
  tooltip; ↑ ↓ Home End move the selection. With nothing selected the
  detail is a status page with the same ways to add. The detail's header
  bar carries the tabs (Run, Runs or Threads, App, Definition), the run
  kind's buttons (Open in Chat, Start run, List only and Dry run, Stop
  while a run is in flight) and the agent's menu (Enable or Disable, the
  two exports, Duplicate, Copy and Rotate token, Reset to shipped, Delete).
  The Run tab is a preferences page in two columns over 1000 px: the hero
  (state, kind, model, tools, app chips), what needs attention, the run
  kind's group (what keeps a start off, the last run with Open, the
  threads, the mounts a start binds), the config form as boxed rows
  (`widgets/schema_form.rs`, a boolean as a switch, a changed field with
  the accent dot) with the save toast and Ctrl+S, the runtime's bounds and
  the package as rows, the agent's Enabled switch (applies now) and a
  Delete row. Definition keeps its editor with Validate, its own save
  toast and Ctrl+S; Runs, App and a run's review fill the body under the
  header as before, the review's clipped columns narrowing in a narrow
  pane so the column being read keeps room. `scripts/drive/agents-split.json`
  drives the view (it saves a config field: a dev copy only), and
  `scripts/drive/agent-fake.js` fakes a run to review and a tool gap in
  the page. Not done yet: the Runs and Threads tables, the App tab's bar
  and drawers, the review and the Definition tab's notices keep their
  inner layouts (no list rows or preference rows yet); the Import,
  Install, New, Duplicate and Rotate dialogs keep their forms; config
  numbers are text boxes, not spin buttons; the header's tabs and buttons
  do not fold into a menu yet when a pane is narrower than about 600 px.
- **Upstreams and MCP servers** (7 above, 2026-11-15): both on the shared
  split view (`nav_split`), the selection in the path on one route each:
  `/upstreams/:id?` (`/upstreams/3`, `/upstreams/new`) and
  `/mcp-servers/:id?/:label?` (`/mcp-servers/3`, `/mcp-servers/new`,
  `/mcp-servers/builtin/lmgw`). The two list panes share one component
  (`pages/collection.rs`): the title with a live summary under it, + to
  add, the search (Upstreams `?q=`, MCP servers `?find=`; `/`, Ctrl+F,
  Esc), the rows as the kit's list rows with a state dot and a line of
  facts (each with its whole text as a tooltip, `ListRow`'s `title`),
  ↑ ↓ Home End Enter as on Models. An upstream's detail is a hero (protocol
  and kind, state: ready, asking, out of quota, unreachable, disabled; its
  catalog's size as a link to Models → Upstream catalogs; whether a key is
  stored; the not-OpenAI's-host note), what lmgw remembers of it (out of
  quota with Check now, a llama.cpp server's GET /props facts per model),
  and its form as preference groups: Connection (enabled, name, protocol,
  kind, base URL), Authentication (the key, empty keeps it; the headers),
  Limits (the timeout), Catalog (passthrough and its prefix, every output
  modality and native /v1/responses on an OpenAI-compatible row), Pricing
  (a link to its rows in Usage → Prices); Test and the menu (Enable or
  Disable, its catalog, Prices, Delete) in the header. An MCP server's
  detail is a hero with its live state (ready with how many of its tools
  are offered, open tasks, transport, prefix), the form (Server; Process
  for stdio: image, command, arguments, environment, working directory,
  podman args; Connection for http and sse: URL, headers; Behaviour:
  prefix, timeout, idle stop, autostart, sampling and its model) and its
  tools as a boxed group of rows with their switches (applying at once),
  Enable and Disable all on its heading. A built-in toolset's detail is
  its tools; with nothing selected the detail is the whole tool inventory,
  one folded group per source, filtered by `?q=` and `?show=`. Both forms
  keep the payloads the modal editors posted (`upstream_set_full`,
  `mcp_server_set`), with change dots, the save toast, Ctrl+S, the dirty
  guard, and a reload that moves the values the owner did not touch.
  `scripts/drive/upstreams-split.json` (with `upstreams-fake.js` for the
  quota and llama.cpp states) and `scripts/drive/mcp-split.json` drive
  them (they save: a dev copy only). Not done yet: an MCP server's Test
  answer is a toast, not a row of the detail; the timeouts are text rows,
  not spin buttons; an agent's own server registration is said in a note
  but its fields stay editable; the lists have no filter by state yet.
- **Engines, Downloads, Upstream catalogs, Find a model** (8 above,
  2026-11-15): Engines is a list beside its detail on the shared split view
  (`pages/backends/list.rs`, `detail.rs`; the divider kept as
  `lmgw.ui.engines.list.width`): the list head has New build as + and a menu
  with Check for updates now and Read again, the search (`?q=`, `/`, Ctrl+F,
  Esc), the builds as list rows (the state dot and word of the live or last
  run, the engine, the ref, the extras, a waiting update), then Local images
  and, where the engines run natively, Release builds; ↑ ↓ Home End move the
  selection. The selection is the path (`pages/backends/route.rs`):
  `/backends/builds/<id>`, `/backends/images` and `/backends/windows` (the
  old tabs' addresses), `/backends` the overview (the counts, Check now and
  the repository presets to start a build from). A build's detail has Run
  (Log while a run is live) and Edit in its header bar, the rest in its menu;
  the hero says the run's state, its age, the engine and the update chips; a
  live run is a card with its phase, the bar and podman's last line; Source,
  Image (the moving tag with Copy), Updates (Check now on the heading), Runs
  (the history table in its box across both columns, paged) and Delete are
  boxed groups. Local images are a boxed group per engine (llama.cpp,
  audio.cpp, sd.cpp, then images no engine claims; its heading with how many
  and their size; lmgw's own first in each), every image a row: the whole
  tag on the first line (the repository muted, wrapping rather than cut),
  its other tags under it, then the backend, size, age and where it came
  from, then the run's or the registry check's state chip and what uses it
  as chips (or a quiet "unused"); Pull update, Delete and the menu at the
  end of the first line. The disk totals stay under the groups. The release
  builds are their table in a box on a wide flow. Downloads lists the files as boxed groups of rows: In
  progress (a bar and the bytes under the name, Stop as an icon), Needs
  attention (failed, changed on Hugging Face, gone from the disk: Retry,
  Update, Download again as icons), then each repo's downloaded files,
  folded from its heading, and the single-file repos together. The Upstream
  catalogs page has its own title (its counts as the subtitle, Refresh and
  Models as icon buttons) instead of the old Models tabs, its table in a
  box. Find a model is search first: a rounded search field with Find as
  the pill, the card and the other choices boxed. `scripts/drive/
  engines-fake.js` fakes populated Engines and Downloads (builds, runs,
  images, downloads, a live build run and a live transfer on the event
  stream; every write refused) and `scripts/drive/engines-split.json` drives
  them. Not done yet: the build editor is still the modal of cards it was
  (not preference rows in the detail), and so are the run log (opened from
  the live card and the header), Check merge, Delete, Retag and Pull update;
  an image has no detail of its own yet;
  Downloads has no pause apart from Stop (Stop keeps the entry for Resume, as
  before); the catalog keeps its rail and table layout.
- **Docs and Knowledge** (9 above, 2026-11-15): both collections on the
  shared split view. Knowledge is one route, `knowledge/:id?/:tab?`
  (`/knowledge/<id>`, `/knowledge/<id>/search`, `/knowledge/<id>/settings`;
  divider kept as `lmgw.ui.knowledge.list.width`): the list has the title
  with how many bases and files, New knowledge base and the search (`?q=`,
  `/`, Ctrl+F, Esc), a row per base with its state and its files, size,
  chunks and model, ↑ ↓ Home End moving the selection; the detail's header
  bar carries the Files, Search and Settings tabs and a menu (Resume,
  Cancel the job, Delete), the hero the base's state, files, size, chunks,
  what is resident, its pin and input limit as chips, its notes under it.
  Files is a boxed list of the files (kind, size, pages, chunks, state,
  reason, menu) that takes dropped files, with the live job and Resume as
  rows; Search is the query field first, the excerpts as rows, the trace
  folded under them; Settings is the base's fields as preference rows
  (`pages/knowledge_form.rs`, shared with the New dialog) with change dots,
  the save toast saying what Save starts ("Save and re-chunk"), Ctrl+S, the
  dirty guard and a Delete row that asks; a save rebases the form to what
  it sent, a new embedding model held as saved until its re-embed job has
  re-pinned the base, and a re-read moves every field not being typed.
  A row's tooltip is its name, state and facts in full. Docs keeps its header bar over
  four tabs (the rerank stage and the resident size as its subtitle,
  Import, Export and Ingest a library as icon buttons); Corpora is the
  split inside the page body (`/docs`, `/docs/corpora/<id>`, divider
  `lmgw.ui.docs.list.width`; the Corpora tab leads back to the corpus shown
  last), its list with the search and the badge filter (`?show=`), the
  corpus a preferences page with no header of its own (a back row when
  narrow): the hero with its state, `docs__resolve`'s badges, chunks,
  resident size and score, then Ingest (the live job, sources, Re-ingest),
  Models (Re-embed), Eval (Run eval, the eval tab and the playground for
  it), Documents (Browse, Export) and Delete. Requests is a boxed list of
  rows under its status toggle; the playground is search first, its stage
  parameters folded into preference rows; the eval dashboard's head is
  rows, its tables boxed; the ingest wizard, the golden query editor and
  the Re-embed, Delete, Export, Import and New knowledge base dialogs are
  boxed groups with pill buttons. `scripts/drive/knowledge-split.json`
  drives Knowledge on a dev instance with the mock embedder (it saves),
  `scripts/drive/docs-split.json` drives Docs on `scripts/drive/docs-fake.js`
  (no ingest, no model). Not done yet: the corpus browser keeps its
  dialog with the old split and chunk cards; the eval dashboard's golden,
  candidate and run tables are still tables (boxed), and its sample and
  per-chunk fields are plain number boxes beside Generate; the playground's
  numbers are text rows, not spin buttons; a base's Settings has no
  hero-level summary of what it is pinned to beyond the chips; the corpora
  have no editable settings (the API has none), so their page has no save
  toast.
- **Traffic, Conversations, Benchmarks, Wiring and the API reference**
  (10 above, 2026-11-15): the record views are lists beside the opened
  record on the shared split view, their list panes built from one module
  (now the shared list pane, below: the header bar, the search with `/`,
  Ctrl+F and Esc, ↑ ↓ Home End and Enter). The selection is the path:
  `/traffic/<log id>`, `/traffic/conversations/<chain id>`,
  `/benchmarks/<run id>`, `/api-reference/<operation id>`; the old
  `?chain=`, `?run=` and `?op=` links land there, and each list keeps its
  query (Traffic's filters, `?q=`) as the selection moves. Requests is the
  log, newest first under sticky day headings, the live tail and "Load n
  older" as before (no cap: the page size is the tail's length and says
  so), the model search with its known aliases, the other filters in a
  popover from the header (upstream, class, key, errors only, rows per
  page), a link's filters as chips; the header has the GPU (a popover with
  the panel, a count while anything waits; with nothing opened it is on the
  status page) and Pause or Resume. An opened request is a hero and its
  stored fields as boxed groups (the error with its message as a code
  block, the request, the answer, tokens and cost), read by its id when the
  list does not hold it, and its menu narrows the list to its model,
  upstream or error kind. Requests and Conversations switch under the
  list's title. A conversation is a hero, its facts and one boxed group
  per turn (text, what it waits for, its JSON folded); Evict by rules,
  Clear all and Delete are in the menus with their confirmations.
  Benchmarks lists the runs (state dot and word, live phase and percent,
  a line of facts, every headline number in the tooltip); the layers
  button turns a click into a pick for Compare, the foot opens the
  comparison. A run and a comparison open in the detail pane under a hero
  (Benchmark again, Cancel while running, Compare with previous, Notes and
  Delete in the menu), their sections, charts and tables as before in one
  column as wide as the pane. Nothing opened shows what a run measures,
  the run going now and the threshold. The API reference lists the
  operations with their method's badge under their group and tag
  headings, the group and method filters in a popover; the opened
  operation holds its chips, Copy as curl and Send in the header bar (the
  chips give way first in a narrow head), then a hero, who it sends as,
  the curl, the documentation (its parameters one line each, name · where
  · type, the description under it) and the tester; the two documents
  to save are in the menu and on the status page, with the routes it does
  not describe. Wiring keeps its tables, now boxed groups under section
  headings. `scripts/drive/records-fake.js` fakes a request log,
  conversations and benchmark runs in the page (no model, no GPU; the
  bench ops that change something are refused), and
  `scripts/drive/traffic-split.json` drives the Traffic and Conversations
  views. Not done yet: the documentation and the tester of an operation
  keep their inner layout (sections, fields) inside the new frame; a run's
  sections are folding sections with boxed cards, not preference groups;
  the New benchmark and notes dialogs keep their forms; Wiring has no list
  and detail (it is a read-only table of every link); the GPU panel keeps
  its own inner layout.
- **Usage and Overview** (11 above, 2026-11-15): Keys & budgets is a list
  beside the selected key on the shared split view (`pages/usage/keys.rs`,
  `pages/usage/keys/`), one route, `usage/keys/:id?`, so `/usage/keys/<id>`
  selects its row (the divider kept as `lmgw.ui.keys.list.width`, one pane
  at a time under 800 px with a back button). The list has the title with
  how many keys it shows, the add menu (New key, Pair a device), the Usage
  views (Charts, Keys, Prices) as a switch under it, the search (`?q=`, `/`,
  Ctrl+F, Esc) and a row per key under its kind's heading (owner keys,
  client keys, devices, agent tokens, internal identities): the kind's
  icon, the name without its kind's prefix, a state (active, disabled,
  paused, expired, over budget; a device online or offline) and a line of
  spend, scope and last use; ↑ ↓ Home End move the selection. With nothing
  selected the detail is a status page: the global budget, the New key form
  as rows (a client key can be fenced before it exists), pairing a device
  with the link strip, and the transfers between devices. A key's detail is
  a hero (kind, who made it, chips for its state, scopes, rate limits,
  expiry, a device's grants and admin tools), the plaintext a create
  returned (shown once, with Copy and Dismiss), Spend (the meter against
  its budget, requests in its period, last use, its requests in Traffic),
  the policy as boxed rows with the save toast, change dots, Ctrl+S and
  the guard (Access, Scope with the alias and tool pickers as rows and the
  preview of the tools it sees, a device's Pairing URLs and hosted label,
  Budget and rate limits), a device's Connection (state, who paired it,
  the pairing notes) and Hosted tools and admin tools (the level as a
  dropdown that applies at once, a raise asking first), and the rows that
  end it (Copy and Rotate an owner key, Revoke, a device's New token,
  Disable or Enable and Delete), each behind its question; the header's
  menu holds the same. The policy dialog is gone: its `key_set` body is
  the form's. Charts, Prices and the Overview sit on the view's tone with
  their tiles and chart cards as boxes on it; the Prices table is in a
  boxed group (scrolling sideways inside it), Sync and Add are icon
  buttons, `?q=` still filters it (the Upstreams page links
  `/usage/prices?q=<upstream>`), and its editor is boxed rows (an existing
  row's scope, unit and tier shown, not editable). The Overview keeps its
  layout (`.ov-state`, `.ov-machine` first, `.ov-find`) with sentence-case
  tiles. `scripts/drive/keys-split.json` drives the Keys view (it creates,
  saves and revokes a key: a dev copy only). The pairing dialog's form
  step is preference rows (Device, What it may reach). Not done yet: the
  pairing review's table, the QR card, New token, the rotate and grant questions keep their
  dialog forms; the budget, rate limits and expiry are text rows, not spin
  buttons or a date picker; the Charts' filter row and cards keep their
  inner layout; the Overview's sections are the folding sections they
  were, not boxed groups; the Prices table has no list and detail.

## Design system (locked 2026-08-29, sample: design-sample.html; superseded in parts by the modern design above)

- **Themes**: dark-first cool graphite (`#16191C` base, cool off-white text), light
  Breeze-like neutral gray. Desktop-native, not editorial: never warm paper/cream tints
  (revised 2026-08-31 — the original warm palette read as a print page); never pure black.
- **Type**: IBM Plex Sans (UI, 400/500/600) + IBM Plex Mono (all data: model names,
  paths, logs, numbers). Bundled woff2, no CDN. Display = Plex Sans 600, tight tracking.
- **Color roles**: `accent` (Breeze-family blue, `#3DAEE9` dark / `#1F6DAD` light) =
  interactive only; `amber` = liveness (downloads, streaming, pulse); `green`/`red` =
  health/error status only. Status hues are signal-strength, not pastel. Model identity
  chips get a deterministic hue from the alias name hash.
- **Shape**: round foundation — controls r6, dense cards r10, airy cards r14, pill chips.
- **Density**: one component set; dense is the `:root` default (admin), `.density-airy` (user)
  flips spacing/control-height/type-size custom properties. No second component set.
- **Layout**: undecorated window → 40px titlebar (drag region: brand, ambient traffic
  pulse, window controls) + grouped left sidebar (eyebrow labels Use / Serve / Connect /
  Observe / Settings) + content pane. Master-detail inside pages; no infinite scroll —
  real tables with virtualization/pagination.
- **Signature**: the titlebar pulse — a small live sparkline of gateway traffic, amber
  when generating/downloading, calm dot when idle. Fed by the global SSE event stream.

## Build & CI

- Toolchain additions: `wasm32-unknown-unknown` target + `trunk` in
  `ci/install-build-deps.sh` (pinned prebuilt binary + sha256, not `cargo install` —
  building trunk from source pulls libdeflate-sys, which does not compile with
  Fedora's GCC 16); `ci/build.sh` runs `trunk build --release` for `lmgw-ui`
  before `cargo build`. CI: the project's existing runner.
- UI check scripts: `scripts/ui-matrix.py` (layout verdict per route and window size),
  `scripts/ui-drive.py` (scripted clicks) and `scripts/webkit-check.py`. `scripts/ui-shots.sh <out-dir>`
  runs the matrix over every route on a throwaway dev copy (GPU hold on) at 125% in dark and light
  and leaves one screenshot per route plus the verdict; the copy is deleted on every exit path.
- Verify current Leptos / Trunk versions via docs at implementation start (Leptos moves
  fast; don't trust memory).

## Phases

Each phase ends compiling, verified, and committed. Old UI keeps working throughout.

Progress: P0 ✅ · P1 ✅ · P2 ✅ (audio model CRUD deferred to P6) · P3 ✅ · P4 ✅ ·
P5 ✅ (chat; the deferred parity gaps closed 2026-08-29: highlight.js code
blocks with copy/preview toolbar + sandboxed HTML preview, `?t=` deep link and
URL sync, IR tool-card replay on reopen) ·
P6 ✅ (audio model CRUD on the Models page, Audio lab surface, container
lifecycle controls; live-container E2E done 2026-08-29: real TTS + ASR through
a dev-named audio.cpp container) ·
P7 ✅ (Wiring ported 2026-08-29 — `GET /api/wiring` + the chain view, plus the
Overview connect snippets; Workflows/mail ported 2026-08-29 — new
`GET /api/workflows/mail{,/stream,/message}` + `POST /api/op/mail_{fetch,
classify,reclassify,apply}` in `web/api_workflows.rs`, the JSON twin of the
htmx page: IMAP, classification, the job registry and the SSE state machine
(`workflows::job_frames`) are shared, not duplicated. The last bridge anchor
in the sidebar is gone — every page is now native. Titlebar pulse shows real
live ~tok/s: in-flight stream progress counted in the proxy drain, 2s rate
window, 500ms stats ticks only while active) ·
P8 ✅ (cutover 2026-08-29 — see the P8 entry below).
Container ops from a dev instance: shut the production lmgw down first and use
a dev-unique container_name (dev names collide with production otherwise).

- **P0 — skeleton**: `lmgw-ui` + `lmgw-api-types` crates, Trunk build, embedded serving
  at `/ui`, app shell (sidebar, routing, light/dark, titlebar), design tokens + the
  two-density component base. One local HTML token/component sample page first to lock
  the visual language before Leptos components exist.
- **P1 — API foundation + Dashboard**: `/api` scaffolding (error model, SSE helpers),
  status endpoint, live dashboard page as the proving ground for the live-data patterns.
- **P2 — Models domain** (the heart): unified models list, local model config editor,
  HF wizard (search → quant → download → auto-plan → auto-create), downloads panel with
  live progress, HF update tracking, explicit apply + test panel.
- **P3 — Upstreams · MCP · Settings**.
- **P4 — Traffic**: live log tail, request/response detail, responses view.
- **P5 — Chat** (first minimal-density surface, live generation view).
- **P6 — Audio + Audio Lab**.
- **P7 — Workflows/mail · Wiring · Dashboard polish**.
- **P8 — Cutover** ✅: SPA at `/` (Trunk `public_url = "/"`, `web/ui.rs` owns `/` plus a
  root catch-all that serves the shell for client-side routes and 404s under `/api/`,
  `/v1/`, `/chat/api/`, `/audio-lab/api/`); `/ui*` → 308 to the same path at the root.
  Deleted: `crates/lmgw-core/templates/` (34 files), `crates/lmgw-core/assets/` (14
  files), `web/mcp.rs`, and every page handler in `web/{admin,audio,embed,hf,responses,
  wiring,workflows,chat,audio_lab}.rs` — those modules now hold only the internals the
  `/api` plane and `crate::ops` call. `askama` dropped from `lmgw-core`. Docs + `ci/check.sh`
  updated; `web_pages.rs` rewritten against the `/api` plane.

  **Known gaps the cutover exposed** (features the old UI had that the new plane had not
  grown yet — all five are closed now; kept as the record of what was ported when):
  1. ~~audio.cpp spec catalog~~ — closed. `GET /api/audio/catalog` serves the *cached*
     snapshot (memory → kv, never an implicit fetch) mapped to families/packages with
     install state, on-disk size and the editor prefill; `POST /api/op/audio_catalog`
     (`action: refresh|download`) is the old `/audio/catalog/{refresh,download}` pair —
     refresh live-fetches `model_specs/*.json` and persists it, download queues the
     package's files through the shared HF queue (target `audio`), fire-and-forget, with
     progress read back from `/api/hf/downloads`. The Models page's audio section gained
     **Add from catalog**: families grouped (installed/served first, auto-open), packages
     with format·precision/file count/size/state, amber live progress, and a "Create
     model" action that opens the audio editor prefilled — the row is never auto-created,
     mirroring the old per-package "serve" form. `LMGW_AUDIO_CATALOG_ENDPOINT` overrides
     the catalog host (mock catalog in `scripts/mock-audio-catalog.py`, E2E in
     `tests/it/audio_catalog.rs`).
  2. ~~Hide/unhide passthrough models~~ — closed. `POST /api/op/model_visibility`
     (`action: hide|unhide`, `upstream_id` + `model_id`) wraps
     `store::{hide,unhide}_passthrough_model`; `/api/models/full`'s `passthrough` entries
     now carry `id` + `hidden`, and the Models page lists each passthrough upstream's live
     catalog with a hide/unhide affordance — a hidden row stays listed, dimmed, rather than
     vanishing.
  3. ~~Duplicate a local model~~ — closed. `local_model_set` gained a `duplicate` action
     (`ops.rs`), mirroring the pre-P8 `unique_copy_id` semantics exactly: a fresh
     `<id>-copy`/`-copy-2`/… id, GGUF/params/args/enabled/public all carried over verbatim.
     Wired to a row action on the Models page; `local_duplicate_clones_under_fresh_id`
     restored in `web_pages.rs`.
  4. ~~Responses store switch + eviction rules~~ — closed. Was already reachable via
     `settings_set_full`; the SPA Settings page gained a "Responses storage" section
     (store on/off, evict-after-idle-hours, max-chains) next to Tokens & updates.
  5. ~~`client_key` on a request log row~~ — closed. `dto::RequestRow` and
     `telemetry::RequestSummary` both carry it now (populated at every `record_*` site in
     `proxy.rs`/`mcp/ingress.rs`), so it rides `/api/logs` and the live `request` SSE frame
     alike; the Traffic page's detail drill-in shows it when non-empty.
