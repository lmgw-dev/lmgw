// folder-chat UI. No build step and no dependencies: this file is compiled
// into the binary and served from the app's own origin (the CSP allows
// nothing else).
//
// Two rules hold everywhere below:
// - Every POST carries `X-Folder-Chat: 1` and `Content-Type: application/json`
//   (the server's CSRF rules; see src/server.rs).
// - Model output is never put into the page as HTML without being escaped
//   first. The markdown renderer escapes all text, then adds a fixed set of
//   tags (code, bold, lists, http(s) links, citation chips) around what it
//   escaped. Reasoning, excerpts, paths and error messages go in as text.
"use strict";

const $ = (id) => document.getElementById(id);
const POST_HEADERS = {
  "Content-Type": "application/json",
  "X-Folder-Chat": "1",
};

// ---------------------------------------------------------------- formatting

const NBSP = "\u202f"; // narrow no-break space: "32 768"
function num(n) {
  if (n === null || n === undefined) return "–";
  return Number(n).toLocaleString("en-US").replace(/,/g, NBSP);
}
function bytes(b) {
  if (b === null || b === undefined) return "–";
  const units = ["B", "KiB", "MiB", "GiB"];
  let v = b, u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u += 1; }
  return (u === 0 ? String(v) : v.toFixed(1)) + " " + units[u];
}
function when(ms) {
  if (!ms) return "never";
  const d = new Date(ms);
  const secs = Math.round((Date.now() - ms) / 1000);
  let ago;
  if (secs < 60) ago = "just now";
  else if (secs < 3600) ago = Math.round(secs / 60) + " min ago";
  else if (secs < 86400) ago = Math.round(secs / 3600) + " h ago";
  else ago = Math.round(secs / 86400) + " d ago";
  return d.toLocaleString() + " (" + ago + ")";
}
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

// ---------------------------------------------------------------- safe markdown

function esc(s) {
  return s.replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

// Applied to text that is ALREADY escaped: links (http and https only),
// citation chips for numbers the answer's metadata knows, and bold. `[`, `]`,
// `(`, `)` and `*` survive escaping unchanged, so the patterns still match;
// `"` does not, so an href can never close its attribute.
const SPAN_RE = /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)|\[(\d+(?:\s*,\s*\d+)*)\]|\*\*(.+?)\*\*/g;
function spans(escaped, cites) {
  return escaped.replace(SPAN_RE, (m, linkText, url, nums, bold) => {
    if (url) {
      return '<a href="' + url + '" target="_blank" rel="noopener noreferrer">' + linkText + "</a>";
    }
    if (nums) {
      return nums.split(/\s*,\s*/).map((n) => cites.has(Number(n))
        ? '<button type="button" class="cite" data-n="' + Number(n) + '">' + Number(n) + "</button>"
        : "[" + n + "]").join("");
    }
    return "<strong>" + spans(bold, cites) + "</strong>";
  });
}

// Inline code first (its content is escaped and nothing else touches it),
// then the spans on everything between. An unmatched backtick stays a
// backtick.
function inline(text, cites) {
  const parts = text.split("`");
  let html = "";
  for (let k = 0; k < parts.length; k++) {
    const isCode = k % 2 === 1 && k < parts.length - 1;
    if (isCode) html += "<code>" + esc(parts[k]) + "</code>";
    else html += (k % 2 === 1 ? "`" : "") + spans(esc(parts[k]), cites);
  }
  return html;
}

function renderMarkdown(src, cites) {
  const out = [];
  const lines = src.split("\n");
  let para = [];
  let list = null;
  const flushPara = () => {
    if (para.length) out.push("<p>" + para.map((l) => inline(l, cites)).join("<br>") + "</p>");
    para = [];
  };
  const flushList = () => {
    if (list) {
      out.push("<" + list.type + ">" +
        list.items.map((it) => "<li>" + inline(it, cites) + "</li>").join("") +
        "</" + list.type + ">");
    }
    list = null;
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = line.match(/^\s*(`{3,}|~{3,})/);
    if (fence) {
      flushPara(); flushList();
      const code = [];
      i += 1;
      // An unclosed fence (still streaming) runs to the end.
      while (i < lines.length && !lines[i].trim().startsWith(fence[1])) { code.push(lines[i]); i += 1; }
      out.push('<pre class="code"><code>' + esc(code.join("\n")) + "</code></pre>");
      continue;
    }
    const item = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
    if (item) {
      flushPara();
      const type = /\d/.test(item[1]) ? "ol" : "ul";
      if (!list || list.type !== type) { flushList(); list = { type, items: [] }; }
      list.items.push(item[2]);
      continue;
    }
    if (list && /^\s{2,}\S/.test(line)) {
      list.items[list.items.length - 1] += " " + line.trim();
      continue;
    }
    const heading = line.match(/^#{1,6}\s+(.*)$/);
    if (heading) {
      flushPara(); flushList();
      out.push('<p class="h">' + inline(heading[1], cites) + "</p>");
      continue;
    }
    if (line.trim() === "") { flushPara(); flushList(); continue; }
    flushList();
    para.push(line);
  }
  flushPara(); flushList();
  return out.join("");
}

// ---------------------------------------------------------------- SSE over fetch

// EventSource cannot POST, so the chat stream is read by hand: events end at
// a blank line, `data:` lines join with "\n", comments are keep-alives.
async function readSse(body, onEvent) {
  const reader = body.getReader();
  const dec = new TextDecoder();
  let buf = "", name = "message", data = [];
  const line = (l) => {
    if (l === "") {
      if (data.length) onEvent(name, data.join("\n"));
      name = "message"; data = [];
    } else if (l.startsWith(":")) {
      // keep-alive
    } else if (l.startsWith("event:")) {
      name = l.slice(6).trim();
    } else if (l.startsWith("data:")) {
      data.push(l.slice(5).replace(/^ /, ""));
    }
  };
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      let l = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (l.endsWith("\r")) l = l.slice(0, -1);
      line(l);
    }
  }
  if (buf) line(buf);
  line("");
}

// ---------------------------------------------------------------- status

// The last /api/status body. Not `status`: that name is window.status.
let appStatus = null;

async function refreshStatus() {
  try {
    const res = await fetch("/api/status", { cache: "no-store" });
    if (!res.ok) return;
    appStatus = await res.json();
  } catch (_) {
    return;
  }
  const s = appStatus;
  $("k-files").textContent = s.ready ? num(s.documents) : "no index loaded";
  $("k-chunks").textContent = s.ready ? num(s.chunks) : "–";
  $("k-embed").textContent = (s.embed_model || s.config.embed_model + " (not loaded)") +
    (s.unverified ? " — " + s.unverified : "");
  $("k-resident").textContent = s.ready ? bytes(s.resident_bytes) : "–";
  $("k-last").textContent = when(s.last_sync_finished_ms);
  const rr = s.config.rerank_model ? " · rerank " + s.config.rerank_model : "";
  const vis = s.config.vision_model
    ? " · vision " + s.config.vision_model + (s.config.vision_every_page ? " (every page)" : "")
    : "";
  $("models").textContent = "chat " + s.config.chat_model + " · embed " + s.config.embed_model + rr + vis;
  updatePill();
  if (!lastBudget) renderBudget(null);
}

function updatePill() {
  const p = $("state-pill");
  if (sync.running) { p.className = "pill syncing"; p.textContent = "syncing"; }
  else if (appStatus && appStatus.ready) { p.className = "pill ready"; p.textContent = "ready"; }
  else { p.className = "pill empty"; p.textContent = "no index yet"; }
}

// ---------------------------------------------------------------- sync panel

const sync = {
  run: null, running: false, planned: null, settled: 0, current: null,
  // The page a vision model is reading now (a `reading` event): seconds per
  // page, so the card says so rather than look stuck.
  reading: null,
  skips: {}, ignoredDirs: [], ended: false,
  // Set when the event stream (re)connects: the server replays the run from
  // its first event, so the panel starts over rather than appending twice —
  // run numbers restart with the process, so the number alone cannot tell.
  fresh: true,
};

function resetSync(info) {
  sync.run = info.run;
  sync.planned = null;
  sync.settled = 0;
  sync.current = null;
  sync.reading = null;
  sync.skips = {};
  sync.ignoredDirs = [];
  sync.ended = false;
  $("sync-notices").replaceChildren();
  $("sync-summary").replaceChildren();
  $("sync-log").replaceChildren();
  $("sync-bar").style.width = "0";
  renderSkips();
}

function notice(container, kind, text) {
  const n = el("div", "notice " + kind, text);
  container.appendChild(n);
  return n;
}

function logLine(cls, tag, what, extra) {
  const log = $("sync-log");
  const nearBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 24;
  const li = el("li", cls);
  li.appendChild(el("span", "tag", tag));
  const w = el("span", "what", what);
  if (extra) { w.appendChild(document.createTextNode(" ")); w.appendChild(el("small", "", extra)); }
  li.appendChild(w);
  log.appendChild(li);
  if (nearBottom) log.scrollTop = log.scrollHeight;
}

function describe(reason) {
  return (appStatus && appStatus.skip_reasons && appStatus.skip_reasons[reason]) || reason;
}

function renderSkips() {
  const table = $("skips");
  const body = table.querySelector("tbody");
  body.replaceChildren();
  const order = appStatus && appStatus.skip_reasons ? Object.keys(appStatus.skip_reasons) : [];
  const reasons = Object.keys(sync.skips).sort((a, b) => {
    const ia = order.indexOf(a), ib = order.indexOf(b);
    return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
  });
  for (const r of reasons) {
    const tr = el("tr");
    tr.appendChild(el("td", "", describe(r)));
    tr.appendChild(el("td", "num", num(sync.skips[r])));
    body.appendChild(tr);
  }
  table.hidden = reasons.length === 0;
  const dirs = $("ignored-dirs");
  const ul = dirs.querySelector("ul");
  ul.replaceChildren(...sync.ignoredDirs.map((d) => el("li", "", d + "/")));
  dirs.hidden = sync.ignoredDirs.length === 0;
}

function renderProgress() {
  const bar = $("sync-bar");
  const label = $("sync-label");
  bar.parentElement.classList.toggle("running", sync.running);
  $("sync-now").disabled = sync.running;
  $("sync-stop").hidden = !sync.running;
  $("sync-state").textContent = sync.run ? "run " + sync.run + (sync.running ? ", running" : "") : "";
  if (!sync.planned) {
    if (sync.running) label.textContent = "Starting: connecting to the embedding model and scanning the folder.";
    else if (!sync.run) label.textContent = "No sync has run since the app started.";
    return;
  }
  const total = sync.planned.new + sync.planned.changed;
  let frac = 1;
  if (total > 0) {
    const partial = sync.current ? (sync.current.batch - 1) / sync.current.of : 0;
    frac = Math.min(1, (sync.settled + partial) / total);
  }
  if (sync.ended) frac = total > 0 ? Math.min(1, sync.settled / total) : 1;
  bar.style.width = (frac * 100).toFixed(1) + "%";
  if (sync.running) {
    let t = num(sync.settled) + " of " + num(total) + " new or changed files";
    if (sync.current) {
      t += " · embedding " + sync.current.file + ", batch " + sync.current.batch + " of " + sync.current.of +
        " (" + sync.current.texts + " texts, batches of " + sync.current.batch_size + ")";
    } else if (sync.reading) {
      const model = appStatus && appStatus.config.vision_model ? appStatus.config.vision_model : "the vision model";
      t += " · " + model + " is reading page " + sync.reading.page + " of " + sync.reading.of + " of " +
        sync.reading.file + " from its image (seconds per page)";
    }
    label.textContent = t;
  }
}

function onRun(info) {
  if (sync.fresh || info.run !== sync.run) resetSync(info);
  sync.fresh = false;
  sync.running = info.running;
  if (!info.running) {
    sync.current = null;
    sync.reading = null;
    refreshStatus();
  }
  renderProgress();
  updatePill();
}

function onSync(ev) {
  switch (ev.type) {
    case "scanning":
      $("sync-label").textContent = "Scanned: " + num(ev.found) + " files of an indexable type.";
      break;
    case "index_reset":
      notice($("sync-notices"), "amber", "The index is being rebuilt from nothing: " + ev.reason + ".");
      break;
    case "planned":
      sync.planned = ev;
      sync.skips = Object.assign({}, ev.skipped_by_reason);
      sync.ignoredDirs = ev.ignored_dirs || [];
      renderSkips();
      logLine("", "plan",
        num(ev.new) + " new, " + num(ev.changed) + " changed, " + num(ev.removed) + " removed, " +
        num(ev.unchanged) + " unchanged");
      break;
    case "reading":
      sync.current = null; sync.reading = ev;
      break;
    case "reading_failed":
      // One page; the file goes on, so it is not settled here.
      logLine("err", "page", ev.file + ", page " + ev.page,
        "not read by " + ev.model + ": " + ev.reason + (ev.cached
          ? ". Not tried again until the file, the vision alias or the prompt changes, or you press Retry failed pages."
          : ". The next sync tries it again."));
      break;
    case "embedding":
      sync.current = ev; sync.reading = null;
      break;
    case "file_done":
      sync.settled += 1; sync.current = null; sync.reading = null;
      logLine("ok", "indexed", ev.file, num(ev.chunks) + (ev.chunks === 1 ? " chunk" : " chunks"));
      break;
    case "unchanged":
      sync.settled += 1; sync.reading = null;
      logLine("", "same", ev.file, "touched, content identical");
      break;
    case "skipped":
      sync.settled += 1; sync.reading = null;
      sync.skips[ev.reason] = (sync.skips[ev.reason] || 0) + 1;
      renderSkips();
      logLine("", "skipped", ev.file, describe(ev.reason) + (ev.detail ? ": " + ev.detail : ""));
      break;
    case "error":
      sync.settled += 1; sync.current = null; sync.reading = null;
      logLine("err", "error", ev.file, ev.message);
      break;
    case "removed":
      logLine("removed", "removed", ev.file);
      break;
    case "done":
      sync.ended = true; sync.current = null; sync.reading = null;
      renderReport(ev.report);
      break;
    case "aborted":
      sync.ended = true; sync.current = null; sync.reading = null;
      // Every kind shows the server's own reason, which says what happened
      // and what to do; the kind only picks the wording around it.
      if (ev.kind === "stopped") {
        // The Stop sync button or shutdown; the reason says which
        // (STOP_ABORT_KIND in server.rs). Deliberate, not a failure.
        notice($("sync-notices"), "", "Sync stopped: " + ev.reason + ". The next sync picks up where it stopped.");
      } else if (ev.kind === "gpu_hold") {
        notice($("sync-notices"), "", "Sync paused: " + ev.reason + ". It resumes where it stopped on the next sync.");
      } else if (ev.kind === "index_containment") {
        notice($("sync-notices"), "err", "The index directory was moved or replaced while the app ran, so the " +
          "sync stopped and nothing was written: " + ev.reason);
      } else {
        notice($("sync-notices"), "err", "Sync stopped (" + ev.kind.replace(/_/g, " ") + "): " + ev.reason);
      }
      $("sync-label").textContent = "Stopped after " + num(sync.settled) + " files.";
      break;
    default:
      break;
  }
  renderProgress();
}

function renderReport(r) {
  sync.skips = Object.assign({}, r.skipped_by_reason);
  sync.ignoredDirs = r.ignored_dirs || [];
  renderSkips();
  $("sync-label").textContent = "Done in " + (r.elapsed_ms / 1000).toFixed(1) + " s.";
  const box = $("sync-summary");
  box.replaceChildren();
  box.appendChild(el("p", "",
    num(r.new_files) + " new, " + num(r.changed_files) + " changed, " + num(r.removed_files) +
    " removed, " + num(r.unchanged + r.touched_unchanged) + " unchanged. " + num(r.embedded_chunks) +
    " chunks embedded in " + num(r.embed_requests) + " requests (batches of up to " +
    num(r.embed_batch_size) + "). The index holds " + num(r.documents) + " files, " + num(r.chunks) +
    " chunks of about " + num(r.chunk_tokens) + " tokens (" + r.token_estimator + ")."));
  if (r.ignore_files && r.ignore_files.length) {
    box.appendChild(el("p", "note", "Ignore rules from: " + r.ignore_files.join(", ")));
  }
  for (const n of r.notes || []) box.appendChild(el("p", "note", n));
  // Failed pages stay failed until their key changes; once the owner fixed
  // what failed (the model's context, its projector), this reads them again.
  if (r.vision_pages_failed && r.vision_pages_failed.length) {
    const row = el("p", "note");
    const b = el("button", "btn", "Retry failed pages");
    b.type = "button";
    b.addEventListener("click", retryFailed);
    row.appendChild(b);
    row.appendChild(document.createTextNode(" " + num(r.vision_pages_failed.length) +
      " page(s) listed above: forgets their failures and syncs, reading them again."));
    box.appendChild(row);
  }
}

// Forgets every cached page failure and starts a sync (POST /api/vision/retry);
// the run's progress arrives on the sync events like any other.
async function retryFailed(e) {
  if (e && e.target) e.target.disabled = true;
  try {
    const res = await fetch("/api/vision/retry", { method: "POST", headers: POST_HEADERS, body: "{}" });
    const b = await res.json().catch(() => null);
    if (!res.ok) {
      $("sync-label").textContent = (b && b.error && b.error.message) || "The failed pages could not be retried (HTTP " + res.status + ").";
      if (e && e.target) e.target.disabled = false;
    }
  } catch (err) {
    $("sync-label").textContent = "The failed pages could not be retried: " + err;
    if (e && e.target) e.target.disabled = false;
  }
  renderProgress();
}

function connectSyncEvents() {
  const es = new EventSource("/api/sync/events");
  es.addEventListener("run", (e) => onRun(JSON.parse(e.data)));
  es.addEventListener("sync", (e) => onSync(JSON.parse(e.data)));
  es.addEventListener("open", () => { sync.fresh = true; });
  es.onerror = () => {
    // EventSource reconnects by itself; the replay on reconnect rebuilds the
    // panel from the run's first event.
    $("sync-state").textContent = "reconnecting…";
  };
}

async function syncNow() {
  $("sync-now").disabled = true;
  try {
    const res = await fetch("/api/sync", { method: "POST", headers: POST_HEADERS, body: "{}" });
    if (!res.ok) {
      const b = await res.json().catch(() => null);
      $("sync-label").textContent = (b && b.error && b.error.message) || "The sync could not be started (HTTP " + res.status + ").";
    }
  } catch (e) {
    $("sync-label").textContent = "The sync could not be started: " + e;
  }
  renderProgress();
}

// Stops the running sync where it stands; the run ends with an `aborted`
// event of kind `stopped` ("stopped by the owner"), and the next sync picks
// up what it left.
async function stopSync() {
  $("sync-stop").disabled = true;
  try {
    const res = await fetch("/api/sync/stop", { method: "POST", headers: POST_HEADERS, body: "{}" });
    if (!res.ok) {
      const b = await res.json().catch(() => null);
      $("sync-label").textContent = (b && b.error && b.error.message) || "The sync could not be stopped (HTTP " + res.status + ").";
    }
  } catch (e) {
    $("sync-label").textContent = "The sync could not be stopped: " + e;
  }
  $("sync-stop").disabled = false;
  renderProgress();
}

// ---------------------------------------------------------------- chat

// The finished turns so far, sent with every question. Not `history`: that
// name is window.history.
let chatHistory = [];
let inflight = null;
let lastBudget = null;

// The largest conversation the server reads for one question, and what it is
// derived from (the model's context × MAX_BYTES_PER_TOKEN; see server.rs).
function bodyLimitText() {
  const l = appStatus && appStatus.chat_body_limit;
  if (!l) return "";
  if (l.error) return "request size limit unknown (" + l.error + ")";
  return "requests up to " + bytes(l.bytes) + " (context " + num(l.context_tokens) +
    (l.context_reported ? "" : ", FALLBACK_CONTEXT_TOKENS") + " × MAX_BYTES_PER_TOKEN " +
    num(l.max_bytes_per_token) + ")";
}

function renderBudget(meta) {
  const box = $("budget");
  box.replaceChildren();
  if (!meta) {
    box.textContent = appStatus
      ? "Chat model " + appStatus.config.chat_model + ". The token budget appears with the first answer." +
        (bodyLimitText() ? " " + bodyLimitText() + "." : "")
      : "";
    return;
  }
  const b = meta.budget;
  const nr = " (not reported by the model)";
  let reserveNote = "";
  if (!b.answer_reserve_reported) {
    reserveNote = b.max_output_tokens === null || b.max_output_tokens === undefined
      ? nr
      : " (DEFAULT_ANSWER_RESERVE_TOKENS: the reported max_output_tokens " + num(b.max_output_tokens) +
        " leaves no room for the prompt)";
  }
  let t = "context " + num(b.context_tokens) + (b.context_reported ? "" : nr) +
    " · answer reserve " + num(b.answer_reserve_tokens) + reserveNote +
    " · system " + num(b.system_tokens) +
    " · history " + num(b.history_tokens) +
    " · excerpts up to " + num(b.retrieval_tokens) + " tokens, estimated as " + b.estimator;
  if (bodyLimitText()) t += " · " + bodyLimitText();
  box.appendChild(document.createTextNode(t));
  if (meta.served_by_fallback) {
    box.appendChild(document.createTextNode(" · "));
    box.appendChild(el("span", "fb", "answered by " + meta.served_by_fallback +
      " (lmgw fell back from " + meta.chat_model + ")"));
  }
}

function addTurn(cls, who) {
  $("conv-empty").hidden = true;
  const art = el("article", "turn " + cls);
  art.appendChild(el("div", "who", who));
  const body = el("div", "body");
  art.appendChild(body);
  $("conv").appendChild(art);
  return { art, body };
}

function scrollConv() {
  const c = $("conv");
  c.scrollTop = c.scrollHeight;
}

function citeText(c) {
  const where = [];
  if (c.page) where.push("page " + c.page);
  else if (c.heading_path) where.push(c.heading_path);
  // Lines of the file as it was indexed (stored by the sync): a file edited
  // since is cited where the excerpt was then, until the next sync.
  if (c.start_line) {
    where.push((c.start_line === c.end_line ? "line " + c.start_line : "lines " + c.start_line + "–" + c.end_line) +
      " as indexed");
  } else {
    where.push("no line numbers stored for this excerpt");
  }
  where.push("score " + Number(c.score).toFixed(3));
  return where.join(" · ");
}

function toggleCite(turn, n) {
  const box = turn.cites;
  const existing = box.querySelector('[data-n="' + n + '"]');
  const chips = turn.answer.querySelectorAll('.cite[data-n="' + n + '"]');
  if (existing) {
    existing.remove();
    chips.forEach((c) => c.classList.remove("open"));
    return;
  }
  const c = turn.citations.get(n);
  if (!c) return;
  const panel = el("div", "cite-panel");
  panel.dataset.n = String(n);
  const where = el("div", "where");
  where.appendChild(el("span", "n", "[" + n + "]"));
  where.appendChild(el("b", "", c.path));
  where.appendChild(document.createTextNode(" — " + citeText(c)));
  panel.appendChild(where);
  panel.appendChild(el("pre", "", c.text));
  box.appendChild(panel);
  chips.forEach((ch) => ch.classList.add("open"));
}

function turnMeta(turn, meta, usage, finish) {
  const box = turn.meta;
  box.replaceChildren();
  if (meta) {
    const parts = [];
    parts.push(num(meta.citations.length) + " of " + num(meta.excerpts_found) + " excerpts used (at most " +
      num(meta.max_excerpts) + ")");
    if (meta.excerpts_dropped_for_budget) parts.push(num(meta.excerpts_dropped_for_budget) + " dropped for the budget");
    if (meta.rerank_model) parts.push("reranked by " + meta.rerank_model);
    else if (meta.rerank_skipped) parts.push("not reranked: " + meta.rerank_skipped);
    if (usage) {
      const u = [];
      if (usage.prompt_tokens !== null && usage.prompt_tokens !== undefined) u.push(num(usage.prompt_tokens) + " prompt");
      if (usage.completion_tokens !== null && usage.completion_tokens !== undefined) u.push(num(usage.completion_tokens) + " answer");
      if (u.length) parts.push(u.join(" + ") + " tokens (reported)");
    }
    box.appendChild(el("div", "", parts.join(" · ")));
    for (const n of meta.notes || []) box.appendChild(el("div", "", n));
  }
  if (finish && finish !== "stop") {
    box.appendChild(el("div", "warn", finish === "length"
      ? "The answer stopped at the model's length limit (finish: length)."
      : "The answer ended with finish reason '" + finish + "'."));
  }
}

function chatNotice(turn, code, message) {
  let kind = "err";
  const tooLong = code === "conversation_too_long" || code === "body_too_large";
  if (code === "gpu_hold" || code === "no_index" || code === "index_model_changed" || tooLong ||
    code === "stopped") kind = "amber";
  const n = notice(turn.body, kind, message);
  if (tooLong) {
    const b = el("button", "btn", "New chat");
    b.type = "button";
    b.addEventListener("click", newChat);
    n.appendChild(el("br"));
    n.appendChild(b);
  }
}

function setSending(on) {
  const send = $("send");
  send.textContent = on ? "Stop" : "Send";
  send.classList.toggle("stop", on);
}

async function ask(question) {
  const u = addTurn("user", "you");
  u.body.appendChild(el("div", "user-text", question));
  const a = addTurn("assistant", appStatus ? appStatus.config.chat_model : "answer");
  const turn = {
    body: a.body,
    thinking: null,
    answer: el("div", "md"),
    cites: el("div", "cites"),
    meta: el("div", "turn-meta"),
    citations: new Map(),
  };
  a.body.appendChild(turn.answer);
  a.body.appendChild(turn.cites);
  a.body.appendChild(turn.meta);
  a.art.addEventListener("click", (e) => {
    const chip = e.target.closest(".cite");
    if (chip) toggleCite(turn, Number(chip.dataset.n));
  });
  a.art.classList.add("streaming");
  scrollConv();

  const ctrl = new AbortController();
  inflight = ctrl;
  setSending(true);
  let answer = "", reasoning = "", meta = null, usage = null, finish = null, failed = null;
  let frame = 0;
  const paint = () => {
    frame = 0;
    turn.answer.innerHTML = renderMarkdown(answer, turn.citations);
    if (turn.thinking) turn.thinking.querySelector("pre").textContent = reasoning;
    scrollConv();
  };
  const schedule = () => { if (!frame) frame = requestAnimationFrame(paint); };

  try {
    const res = await fetch("/api/chat", {
      method: "POST",
      headers: Object.assign({ Accept: "text/event-stream" }, POST_HEADERS),
      body: JSON.stringify({ history: chatHistory, question }),
      signal: ctrl.signal,
    });
    if (!res.ok) {
      const b = await res.json().catch(() => null);
      failed = { code: (b && b.error && b.error.code) || "http", message: (b && b.error && b.error.message) || "HTTP " + res.status };
    } else {
      await readSse(res.body, (name, data) => {
        const d = JSON.parse(data);
        switch (name) {
          case "meta":
            meta = d;
            if (d.served_by_fallback) {
              a.art.querySelector(".who").textContent = d.served_by_fallback + " (fallback for " + d.chat_model + ")";
            }
            for (const c of d.citations) turn.citations.set(c.n, c);
            lastBudget = d;
            renderBudget(d);
            turnMeta(turn, meta, null, null);
            break;
          case "reasoning":
            if (!turn.thinking) {
              turn.thinking = el("details", "thinking");
              turn.thinking.appendChild(el("summary", "", "thinking"));
              turn.thinking.appendChild(el("pre"));
              turn.body.insertBefore(turn.thinking, turn.answer);
            }
            reasoning += d.text;
            schedule();
            break;
          case "text":
            answer += d.text;
            schedule();
            break;
          case "finish":
            finish = d.reason;
            break;
          case "usage":
            usage = d;
            break;
          case "error":
            failed = d;
            break;
          default:
            break;
        }
      });
    }
  } catch (e) {
    if (e.name === "AbortError") failed = { code: "stopped", message: "Stopped. The model was told to stop generating." };
    else failed = { code: "network", message: "The answer stream broke off: " + e };
  }
  if (frame) cancelAnimationFrame(frame);
  paint();
  a.art.classList.remove("streaming");
  turnMeta(turn, meta, usage, finish);
  if (failed) chatNotice(turn, failed.code, failed.message);
  // Only a whole answer joins the history the next question is asked with;
  // its excerpts do not (the server says why in chat.rs).
  if (!failed && answer) {
    chatHistory.push({ role: "user", content: question }, { role: "assistant", content: answer });
  }
  inflight = null;
  setSending(false);
  scrollConv();
}

function newChat() {
  if (inflight) inflight.abort();
  chatHistory = [];
  lastBudget = null;
  const conv = $("conv");
  conv.querySelectorAll(".turn").forEach((t) => t.remove());
  $("conv-empty").hidden = false;
  renderBudget(null);
  $("q").focus();
}

function submit() {
  if (inflight) { inflight.abort(); return; }
  const q = $("q").value.trim();
  if (!q) return;
  $("q").value = "";
  ask(q);
}

// ---------------------------------------------------------------- wiring

document.addEventListener("DOMContentLoaded", () => {
  $("composer").addEventListener("submit", (e) => { e.preventDefault(); submit(); });
  $("q").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); submit(); }
  });
  $("new-chat").addEventListener("click", newChat);
  $("sync-now").addEventListener("click", syncNow);
  $("sync-stop").addEventListener("click", stopSync);
  refreshStatus().then(connectSyncEvents);
  // The "x min ago" of the last sync, kept current.
  setInterval(() => { if (appStatus) $("k-last").textContent = when(appStatus.last_sync_finished_ms); }, 30000);
});
