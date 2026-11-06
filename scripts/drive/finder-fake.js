// In-page fakes for the finder's drive (scripts/drive/finder.json, find-a-model design WP6):
// the answers of POST /api/op/model_find, GET /api/hf/repo (the header's marks and the file
// list), POST /api/op/model_get, /api/op/model_get_status and /api/op/job_cancel, the jobs
// frames of a get on the live feed, POST /api/op/settings_set_full (D9's budget) and
// POST /chat/api/threads ("Say hello"). Nothing reaches Hugging Face, nothing downloads, and no
// model is created or started: every write the finder makes is answered here.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/finder-fake.js`, before the
// page's own scripts, in every document. It is on while sessionStorage's "lmgw.fake.finder" is
// set; the query decides the scene:
//   "demo 8b"      a card by size, then the header's marks: the beside pick (Q4_K_M) and the
//                  whole card's (Q8_0), 25 other models in windows of 10, and 3 partial matches;
//                  the hub's page 2 (cursor "p2") has its own card (Demo 9B), 14 other models (one
//                  of page 1's and page 1's card among them) and 2 partial matches
//   "demo metered" the same card on a metered connection: no header read until "Check exactly"
//   "demo huge"    NothingFits: the card of the model closest to fitting
//   "demo limit"   the hub's limit spent: RateLimited, resets in 40 s
//   "demo nocard"  nothing to measure against (D9), with the hold on (D10)
//   "demo short"   the disk is short of room (D125)
//   "demo private" the card of a private repo: no share link (D124)
//   a pasted share text (WP7): a Hugging Face address with /blob/main/<file> anywhere in the
//                  query, glued to the word before it or not, answers the card for that file
// navigator.clipboard.writeText records its text in window.__finderSeen.copied (WP7).
// A get's frames follow window.__finderStep(n), or run by themselves every 600 ms once
// window.__finderAuto is true; __finderStep("paused") leaves both transfers off the feed with the
// get paused (each file typed `metered` with the server's note), __finderStep("failed") the
// projector's, its row failed, and __finderStep("diskfull") ends the get failed on a full disk with
// the server's sentence. With window.__finderHere set
// before Get it, the projector is on the disk already. A status read answers the get as it was
// when asked, and a cancel's frame follows its answer, as the real feed's can.
// window.__finderSeen records what the fake was asked.
//
// The data is made up and looks it: demo-org, *-demo, sizes rounded.
(() => {
  "use strict";
  if (!sessionStorage.getItem("lmgw.fake.finder")) return;

  // The drive's helpers: an element's text, and a click on the first one holding a text.
  window.__text = sel => (document.querySelector(sel) || {}).textContent || "";
  window.__clickText = (sel, text) => {
    const b = [...document.querySelectorAll(sel)].find(e => e.textContent.includes(text));
    if (b) b.click();
    return !!b;
  };

  const GiB = 2 ** 30, MiB = 2 ** 20;
  // Whole bytes: the page reads them as integers. Every size in GiB, files and graphics-card
  // memory alike, as lmgw says them (owner ruling 2026-11-04, D160).
  const G = n => Math.round(n * GiB);
  const V = G;
  const seen = (window.__finderSeen = { find: [], repo: [], get: [], status: 0, cancel: 0,
    threads: [], settings: [], copied: [], catalog: 0 });
  // On the real Clipboard's prototype, so the page still finds a Clipboard on navigator.
  Clipboard.prototype.writeText = t => { seen.copied.push(t); return Promise.resolve(); };
  const json = (v, status = 200) =>
    new Response(JSON.stringify(v), { status, headers: { "content-type": "application/json" } });
  const wait = ms => new Promise(r => setTimeout(r, ms));
  const bodyOf = (input, init) =>
    (input instanceof Request ? input.clone().text() : Promise.resolve((init && init.body) || "{}"))
      .then(t => { try { return JSON.parse(t || "{}"); } catch (_) { return {}; } });

  // --- fits ---------------------------------------------------------------------------------
  const card = { capacity_bytes: V(24), headroom_bytes: V(0.5), budget: false,
    admission: true, hold: false, outside_bytes: V(2.2),
    residents: [{ class: "audio", model: "voice-demo", bytes: V(1.4) }] };
  const beside = (o) => ({ outside_bytes: V(2.2), residents_bytes: V(1.4), ...o });
  const fit = (mark, measure, o = {}) => ({
    mark, measure, ctx: null, trained_ctx: 131072, uses_bytes: 0, capacity_bytes:
      measure === "beside" ? V(19.9) : V(24), headroom_bytes: V(0.5),
    terms: { weights_bytes: 0, kv_cache_bytes: 0, compute_bytes: 0 },
    beside: measure === "beside" ? beside() : null, bits_per_weight: null, parameters: null,
    notes: [], ...o });
  const header = (measure, bytes, ctx, o = {}) => fit(ctx === 131072 ? "fits" : "fits_shorter",
    measure, { ctx, uses_bytes: bytes, bits_per_weight: o.bpw || 4.9, parameters: 8.03e9,
      terms: { weights_bytes: bytes - V(3), kv_cache_bytes: V(2.4), compute_bytes: V(0.6) },
      ...o });

  // --- the cards ----------------------------------------------------------------------------
  const HOLD = "lmgw is paused on the graphics card right now. The model answers once you " +
    "resume it: the HOLD button at the foot of the sidebar, or the tray menu's GPU hold.";
  const choice = (i, o = {}) => ({ origin: `demo-org/Demo-${i}B-Variant`, name: `Demo ${i}B Variant`,
    author: "demo-org", likes: 400 - i * 7, downloads: 9000 - i * 100, pipeline: "text-generation",
    parameters: i * 1e9, matched_words: 2, gated: false, private: false, note: null, ...o });
  const others = Array.from({ length: 25 }, (_, i) => choice(i + 1,
    i === 3 ? { note: "not available as a file lmgw can run yet" } : {}));
  const dym = [1, 2, 3].map(i => choice(i, { origin: `someone-demo/Demo-Chat-${i}`,
    name: `Demo Chat ${i}`, author: "someone-demo", matched_words: 1 }));
  // The hub's page 2: one of page 1's again and page 1's card among its others.
  const others2 = [choice(3), choice(8, { origin: "demo-org/Demo-8B", name: "Demo 8B" }),
    ...Array.from({ length: 12 }, (_, i) => choice(30 + i))];
  const dym2 = [4, 5].map(i => choice(i, { origin: `someone-demo/Demo-Chat-${i}`,
    name: `Demo Chat ${i}`, author: "someone-demo", matched_words: 1 }));
  const window_ = (all, off, n) => [all.slice(off, off + n), all.length, Math.max(0, all.length - off - n)];

  const baseCard = (upload, o = {}) => ({
    name: "Demo 8B", by: "Demo Org", by_user: null, origin: "demo-org/Demo-8B", likes: 512,
    downloads: 40000, kind: "chat",
    upload, file: "Demo-8B-Q4_K_M.gguf", quant: "Q4_K_M", parts: 1,
    companions: [{ file: "mmproj-Demo-8B-F16.gguf", role: "mmproj" }],
    can: ["reads pictures", "thinks before it answers"], parameters: 8.03e9,
    licence: { id: "apache-2.0", name: null, link: "https://huggingface.co/demo-org/Demo-8B" },
    gated: false, private: false,
    download: { files_bytes: G(5.9), engine_bytes: null, rate_bytes_per_sec: 25 * MiB,
      rate_from: "downloads", secs: 236, sentence: "about 4 minutes on your connection", notes: [] },
    disk_free_bytes: G(410),
    fit: fit("fits_by_size", "beside", { uses_bytes: G(5.9) }),
    whole_card: { file: "Demo-8B-Q8_0.gguf", quant: "Q8_0", bytes: G(8.5),
      fit: fit("fits_by_size", "whole_card", { uses_bytes: G(8.5) }) },
    why: "The largest file of 4 to 8 bits per weight that fits beside what runs now, by its size.",
    share: shareOf("Demo 8B", `https://huggingface.co/${upload}/blob/main/Demo-8B-Q4_K_M.gguf`),
    other_uploads: [{ repo: "someone-demo/Demo-8B-GGUF", likes: 31, downloads: 800, gated: false,
      private: false, plain: true }],
    other_uploads_total: 1, other_uploads_more: 0, more_uploads: false, ...o });

  // The server's share link (lmgw_api_types::model_find::share_text).
  const shareOf = (name, url) => ({ url,
    text: `Get ${name} in lmgw: open Models \u2192 Find a model and paste\n${url}` });

  const ALLOWS = "Without an account it allows 500 requests every 5 minutes; a Hugging Face token in Settings raises that.";
  const budget = (o = {}) => ({ remaining: 480, reset_s: 260, quota: 500, window_s: 300, calls: 6,
    near: false, sentence: null, allowance: ALLOWS, ...o });

  function find(q, a) {
    const base = { query: q, card: null, others: [], others_total: 0, others_more: 0,
      did_you_mean: [], did_you_mean_total: 0, did_you_mean_more: 0, answer: "card",
      sentence: "", next_cursor: null, hub_budget: budget(), notes: [] };
    const p2 = a.cursor === "p2";
    const lists = () => {
      const [o, ot, om] = window_(p2 ? others2 : others, a.others_offset || 0, a.others_count ?? 10);
      const [d, dt, dm] = window_(p2 ? dym2 : dym, a.did_you_mean_offset || 0, a.did_you_mean_count ?? 10);
      return { others: o, others_total: ot, others_more: om, did_you_mean: d,
        did_you_mean_total: dt, did_you_mean_more: dm, next_cursor: p2 ? null : "p2" };
    };
    // A pasted address that names a file: the card for that file of that upload.
    const named = q.match(/https:\/\/huggingface\.co\/([^\s/]+\/[^\s/]+)\/blob\/main\/(\S+?)\.?(?:\s|$)/);
    if (named) {
      const file = decodeURIComponent(named[2]);
      const quant = (file.match(/-([A-Z0-9_]+)\.gguf$/) || [])[1] || null;
      return { ...base, card: baseCard(named[1], { file, quant, whole_card: null, named_file: file,
        fit: fit("fits_by_size", "beside", { uses_bytes: G(8.5) }),
        why: "The file the address names, by its size.",
        share: shareOf("Demo 8B", `https://huggingface.co/${named[1]}/blob/main/${named[2]}`) }) };
    }
    switch (q.trim().toLowerCase()) {
      case "demo private":
        return { ...base, card: baseCard("demo-org/Demo-Private-GGUF", { private: true, share: null }) };
      case "demo 8b":
        if (p2) return { ...base, ...lists(), card: baseCard("demo-org/Demo-9B-GGUF", {
          name: "Demo 9B", origin: "demo-org/Demo-9B", likes: 77, parameters: 9e9 }) };
        return { ...base, ...lists(), card: baseCard("demo-org/Demo-8B-GGUF"),
          notes: ["demo-org/Demo-8B-Instruct was passed over: no upload has a GGUF of it yet."] };
      case "demo metered":
        return { ...base, card: baseCard("demo-org/Demo-Metered-8B-GGUF") };
      case "demo short":
        return { ...base, card: baseCard("demo-org/Demo-8B-GGUF", { disk_free_bytes: G(3.1) }) };
      case "demo nocard":
        return { ...base, card: baseCard("demo-org/Demo-8B-GGUF", {
          fit: fit("no_card", "whole_card", { notes: [HOLD], capacity_bytes: 0, beside: null }),
          whole_card: null, why: "The largest file of 4 to 8 bits per weight, by its size: there is no graphics card to measure against." }) };
      case "demo huge":
        return { ...base, ...lists(), next_cursor: null, answer: "nothing_fits",
          sentence: "None of the 3 models by Demo Org with this name fits your graphics card. The one closest to fitting is Demo 405B, 230 GiB.",
          card: baseCard("demo-org/Demo-405B-GGUF", { name: "Demo 405B", origin: "demo-org/Demo-405B",
            file: "Demo-405B-Q4_K_M-00001-of-00006.gguf", parts: 6, parameters: 405e9,
            companions: [], can: [],
            fit: fit("does_not_fit_by_size", "whole_card", { uses_bytes: G(230) }), whole_card: null,
            download: { files_bytes: G(230), rate_bytes_per_sec: 25 * MiB, rate_from: "downloads",
              secs: 9200, sentence: "about 2 hours 34 minutes on your connection", notes: [] },
            why: "No file of 4 to 8 bits per weight fits the whole card." }) };
      case "demo limit":
        return { ...base, answer: "rate_limited",
          sentence: `Hugging Face asks lmgw to wait 40 seconds. ${ALLOWS}`,
          hub_budget: budget({ remaining: 0, reset_s: 40, near: true, calls: 3 }) };
      default:
        return { ...base, answer: "no_match",
          sentence: `lmgw found no model called “${q}” that it can run. Names are usually written as on the model's page, with numbers as digits.` };
    }
  }

  // --- the header's marks (hf_repo with fit) ------------------------------------------------
  const files = (repo, metered) => {
    const plan = (b, w) => ({ card: null, beside: b, whole_card: w });
    const f = (file, quant, size, fitv, est) => ({ file, role: "weights", quant, parts: 1,
      size_bytes: size, size: `${(size / GiB).toFixed(1)} GiB`, fit: fitv, estimated_from: est || null });
    const bySize = (size) => plan(fit(size < G(19) ? "fits_by_size" : "does_not_fit_by_size", "beside", { uses_bytes: size }),
      fit(size < G(23) ? "fits_by_size" : "does_not_fit_by_size", "whole_card", { uses_bytes: size }));
    const list = metered ? [
      f("Demo-8B-Q4_K_M.gguf", "Q4_K_M", G(5), bySize(G(5))),
      f("Demo-8B-Q8_0.gguf", "Q8_0", G(8.5), bySize(G(8.5))),
      f("Demo-8B-BF16.gguf", "BF16", G(16.1), bySize(G(16.1))),
    ] : [
      f("Demo-8B-Q4_K_M.gguf", "Q4_K_M", G(5), plan(header("beside", V(8.6), 131072),
        header("whole_card", V(8.6), 131072))),
      f("Demo-8B-Q8_0.gguf", "Q8_0", G(8.5), plan(header("beside", V(19.0), 65536, { bpw: 8.5 }),
        header("whole_card", V(17.0), 131072, { bpw: 8.5 }))),
      f("Demo-8B-BF16.gguf", "BF16", G(16.1), plan(fit("does_not_fit", "beside", { uses_bytes: V(21) }),
        header("whole_card", V(22.1), 32768, { bpw: 16 })), "Demo-8B-Q8_0.gguf"),
    ];
    list.push({ file: "mmproj-Demo-8B-F16.gguf", role: "mmproj", quant: "F16", parts: 1,
      size_bytes: G(0.9), size: "0.9 GiB", fit: null, estimated_from: null });
    const pick = (file, measure, why) => ({ file, rule: "whole_context", measure, why, full_context: null });
    return {
      repo, target: "chat", files: list, note: "",
      recommended: metered ? null : "Demo-8B-Q4_K_M.gguf",
      picks: metered ? null : { beside: pick("Demo-8B-Q4_K_M.gguf", "beside",
        "The largest file of 4 to 8.5 bits per weight that remembers the whole conversation beside what runs now."),
        whole_card: pick("Demo-8B-Q8_0.gguf", "whole_card",
          "The largest file of 4 to 8.5 bits per weight that remembers the whole conversation on the whole card.") },
      card, header_bytes_read: metered ? 0 : Math.round(11.0 * MiB),
      fit_notes: metered ? [] : ["The files marked from another file's header are estimated from it."],
      metered: metered ? "The connection is metered (demo0: yes), so lmgw read no file's header: every mark is by its size, and nothing is recommended. Check exactly reads them." : null,
    };
  };

  // NothingFits's upload: one split file, too big by both measures, nothing recommended.
  const huge = repo => {
    const no = m => fit("does_not_fit", m, { uses_bytes: V(216), bits_per_weight: 4.8, parameters: 405e9 });
    const none = m => ({ file: null, rule: "none", measure: m,
      why: "No file of 4 to 8.5 bits per weight fits at least 32 768 tokens.", full_context: null });
    return { repo, target: "chat", note: "", recommended: null, card, header_bytes_read: Math.round(14.2 * MiB),
      files: [{ file: "Demo-405B-Q4_K_M-00001-of-00006.gguf", role: "weights", quant: "Q4_K_M",
        parts: 6, size_bytes: G(230), size: "230 GiB",
        fit: { card: null, beside: no("beside"), whole_card: no("whole_card") }, estimated_from: null }],
      picks: { beside: none("beside"), whole_card: none("whole_card") }, fit_notes: [], metered: null };
  };

  // --- a get ----------------------------------------------------------------------------------
  const JOB = 9001, GET_ID = 77;
  let get = null;   // {phase, done}
  const WEIGHTS = G(5.0), PROJ = G(0.9);
  const job = (id, kind, key, done, total, stage, detail) => ({ id, kind, key, label: kind,
    status: "running", done, total, percent: Math.floor(done * 100 / total), stage, detail,
    error: null, created_at: "2026-11-04 08:00:00", started_at: "2026-11-04 08:00:01",
    finished_at: null });
  // The row's note the boot's metered pause writes (jobs/hf_download.rs PAUSED_METERED_NOTE),
  // and the sentence a get fails with on a full disk (jobs/model_get.rs failed_download over
  // hf_download.rs write_failed).
  const PAUSED_METERED_NOTE = "Paused: metered connection \u2014 lmgw doesn't go on with a " +
    "download by itself on a metered connection. Resume goes on from where it stopped, and so " +
    "does lmgw once the connection isn't metered.";
  const DISK_FULL = (file) => `${file} didn't download: The disk is full: ${file} needs 3.1 GiB ` +
    "more on the disk, 1.2 GiB is free. Free some space; Get it again goes on from the 1.9 GiB already here";
  const detail = (stage, waiting = []) => ({ get_id: GET_ID, repo: get.repo, file: get.file,
    target: "chat", model_id: "demo-8b", stage, downloads: [501, 502], waiting });
  const getKey = () => `model_get:${get.repo}/${get.file}`;
  function frames() {
    if (!get || get.phase === "canceled") return [];
    const p = get.phase;
    if (p === "paused") return [job(JOB, "model_get", getKey(), get.w + get.j, WEIGHTS + PROJ, "paused",
      detail("paused", [{ file: get.file, reason: "metered", note: PAUSED_METERED_NOTE },
        { file: "mmproj-Demo-8B-F16.gguf", reason: "metered", note: PAUSED_METERED_NOTE }]))];
    if (p === "diskfull") return [];             // the download failed: the get ended
    if (p === "failed") return [job(JOB, "model_get", getKey(), get.w + get.j, WEIGHTS + PROJ,
      "downloading", detail("downloading")), job(601, "hf_download", "hf:501", get.w, WEIGHTS, "downloading", {})];
    if (p >= 6) return [];                       // ended: the job left the feed
    if (p === 5) return [job(JOB, "model_get", getKey(), WEIGHTS + PROJ, WEIGHTS + PROJ, "setting up",
      detail("setting_up"))];
    const w = Math.round(Math.min(WEIGHTS, WEIGHTS * p / 4));
    const j = get.here ? PROJ : Math.round(Math.min(PROJ, PROJ * p / 2));
    get.w = w; get.j = j;
    const out = [job(JOB, "model_get", getKey(), w + j, WEIGHTS + PROJ, "downloading", detail("downloading"))];
    if (w < WEIGHTS) out.push(job(601, "hf_download", "hf:501", w, WEIGHTS, "downloading", {}));
    if (j < PROJ) out.push(job(602, "hf_download", "hf:502", j, PROJ, "downloading", {}));
    return out;
  }
  const send = () => window.__frame && window.__frame("jobs", frames());
  window.__finderStep = (phase) => { if (get) { get.phase = phase; send(); } return frames().length; };
  window.__finderAuto = false;
  window.__finderHere = false;
  setInterval(() => {
    if (get && window.__finderAuto && typeof get.phase === "number" && get.phase < 6) {
      get.phase += 1; send();
    }
  }, 600);

  function status() {
    const rows = (w, j) => [{ id: 501, file: get.file, dest_path: "demo/x", status: w },
      { id: 502, file: "mmproj-Demo-8B-F16.gguf", dest_path: "demo/y", status: get.here ? "done" : j }];
    const base = { get_id: GET_ID, repo: get.repo, file: get.file, target: "chat",
      model_id: "demo-8b", public_name: "demo-8b", downloads: rows("downloading", "downloading"), job_id: JOB };
    if (get.phase === "canceled") return { ...base, state: "canceled", downloads: rows("failed", "failed") };
    if (get.phase === "paused") return { ...base, state: "running", stage: "paused",
      downloads: rows("queued", "queued") };
    if (get.phase === "failed") return { ...base, state: "running", stage: "downloading",
      downloads: rows("downloading", "failed") };
    if (get.phase === "diskfull") return { ...base, state: "failed", error: DISK_FULL(get.file),
      downloads: rows("failed", "done") };
    if (get.phase < 6) return { ...base, state: "running", stage: get.phase === 5 ? "setting_up" : "downloading" };
    return { ...base, state: "done", row_id: 31, downloads: rows("done", "done"),
      result: { get_id: GET_ID, model_id: "demo-8b",
      public_name: "demo-8b", target: "chat", row_id: 31, created: true, ctx_size: 65536,
      fit: null, note: { ctx: 65536, trained_ctx: 131072, measure: get.measure || "beside",
        capacity_bytes: V(19.9), headroom_bytes: V(0.5), at: "2026-11-04 08:04:00" },
      warnings: [], next_step: "Say hello" } };
  }

  // --- fetch ------------------------------------------------------------------------------------
  const realFetch = window.fetch;
  window.fetch = function (input, init) {
    const url = new URL(typeof input === "string" ? input : (input && input.url) || String(input), location.href);
    const path = url.pathname;
    // The real catalog answers; the drive counts its reads (D110).
    if (path === "/v1/models") seen.catalog++;
    if (path === "/api/op/model_find") {
      return bodyOf(input, init).then(a => { seen.find.push(a); return wait(500).then(() => json(find(a.query || "", a))); });
    }
    if (path === "/api/hf/repo") {
      const repo = url.searchParams.get("repo") || "";
      const mode = url.searchParams.get("fit") || "false";
      seen.repo.push({ repo, mode });
      const metered = repo.includes("Metered") && mode === "unless_metered";
      const a = repo.includes("405B") ? huge(repo) : files(repo, metered);
      if (mode === "false") { a.recommended = null; a.picks = null; a.card = null; a.header_bytes_read = 0;
        a.fit_notes = []; a.files.forEach(f => { f.fit = null; f.estimated_from = null; }); }
      return wait(mode === "false" ? 200 : 1500).then(() => json(a));
    }
    if (path === "/api/op/model_get") {
      return bodyOf(input, init).then(a => {
        seen.get.push(a);
        get = { phase: 0, repo: a.repo, file: a.file, measure: a.measure, here: !!window.__finderHere,
          w: 0, j: 0 };
        setTimeout(send, 300);
        return wait(300).then(() => json({ job_id: JOB, get_id: GET_ID, row_id: null,
          model_id: "demo-8b", public_name: "demo-8b", target: "chat", measure: a.measure || "beside",
          repo: a.repo, file: a.file,
          downloads: [{ id: 501, file: a.file, dest_path: "demo/x", status: "queued" },
            { id: 502, file: "mmproj-Demo-8B-F16.gguf", dest_path: "demo/y", status: get.here ? "done" : "queued" }],
          companions: [{ file: "mmproj-Demo-8B-F16.gguf", role: "mmproj" }], engine: null,
          disk: { needs_bytes: G(5.9), free_bytes: G(410), short: false, sentence: null, notes: [] },
          joined: false, message: "queued" }));
      });
    }
    if (path === "/api/op/model_get_status") {
      seen.status += 1;
      // The get as it was when asked, answered a little later.
      const s = get ? status() : null;
      return wait(300).then(() => json(s || { error: { code: "not_found", message: "no such get" } }, s ? 200 : 404));
    }
    if (path === "/api/op/job_cancel") {
      seen.cancel += 1;
      // The answer first, the frame that drops the job after it.
      if (get) setTimeout(() => { get.phase = "canceled"; send(); }, 150);
      return wait(100).then(() => json({ ok: true, message: "cancelled" }));
    }
    if (path === "/api/op/settings_set_full") {
      return bodyOf(input, init).then(a => { seen.settings.push(a); return json({ ok: true, message: "Settings saved." }); });
    }
    if (path === "/chat/api/threads" && (init && init.method || (input && input.method) || "GET").toUpperCase() === "POST") {
      return bodyOf(input, init).then(a => { seen.threads.push(a); return json({ id: 4242, title: null, model_alias: a.model_alias }); });
    }
    return realFetch.apply(this, arguments);
  };
})();
