// In-page fakes for the labs' shots (modern design WP5): populated Image, Decision and Voices labs
// without a model, a container or a GPU. It answers, in the page:
//   image:    GET /labs/api/image/groups and /usage — one kept ask with two pictures (SVG data URLs)
//             and one that failed;
//   decision: GET /decision-lab/api/models — a local CPU decision row with its header facts and a
//             cloud one; GET /labs/api/decision/groups and /usage — two asks, one with a choice, a
//             score and a noul answer on two cells (as written, order reversed);
//   voices:   GET /voice-lab/api/model (the facts of a voice-design model) and /voice-lab/api/rounds
//             (one round of three candidates, silent WAVs as data URLs).
// Everything else goes to the dev instance as it is.
//
// Loaded with `scripts/ui-matrix.py --init-script scripts/drive/lab-fake.js` (or ui-drive.py), before
// the page's own scripts. The scenario is sessionStorage's "lmgw.fake.lab", a JSON object
// {image: bool, decision: bool, voices: bool}; `?fake-lab=all` on any URL sets all three. Without
// either nothing is faked. window.__labFakeAnswered counts the answers by key.
//
// The data is made up and looks it: models are *-demo, texts are about example.com tickets.
(() => {
  "use strict";
  let want = null;
  try {
    if (new URL(location.href).searchParams.get("fake-lab") === "all") {
      sessionStorage.setItem("lmgw.fake.lab", JSON.stringify({ image: true, decision: true, voices: true }));
    }
    want = JSON.parse(sessionStorage.getItem("lmgw.fake.lab") || "null");
  } catch (_) {
    want = null;
  }
  if (!want) return;
  const answered = (window.__labFakeAnswered = {});
  const count = (k) => { answered[k] = (answered[k] || 0) + 1; };
  const json = (v, status = 200) => new Response(JSON.stringify(v), {
    status, headers: { "content-type": "application/json" },
  });
  const T = "2026-01-01T12:00:00.000Z";

  // --- media ---------------------------------------------------------------------------------
  const svg = (a, b, label) => "data:image/svg+xml;utf8," + encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512">` +
    `<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="${a}"/>` +
    `<stop offset="1" stop-color="${b}"/></linearGradient></defs><rect width="512" height="512" fill="url(#g)"/>` +
    `<circle cx="256" cy="230" r="120" fill="rgba(255,255,255,.25)"/>` +
    `<text x="256" y="440" font-family="sans-serif" font-size="28" fill="#fff" text-anchor="middle">${label}</text></svg>`);
  // One second of silence, 8 kHz mono 16-bit.
  const wav = (() => {
    const n = 8000, buf = new ArrayBuffer(44 + n * 2), v = new DataView(buf);
    const s = (o, t) => [...t].forEach((c, i) => v.setUint8(o + i, c.charCodeAt(0)));
    s(0, "RIFF"); v.setUint32(4, 36 + n * 2, true); s(8, "WAVE"); s(12, "fmt ");
    v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
    v.setUint32(24, 8000, true); v.setUint32(28, 16000, true); v.setUint16(32, 2, true);
    v.setUint16(34, 16, true); s(36, "data"); v.setUint32(40, n * 2, true);
    let bin = ""; new Uint8Array(buf).forEach((b) => { bin += String.fromCharCode(b); });
    return "data:audio/wav;base64," + btoa(bin);
  })();

  // --- the Image lab -------------------------------------------------------------------------
  const art = (id, run, name, href, size) => ({ id, run_id: run, role: "output", ord: id % 10, name,
    mime: "image/svg+xml", size, sha256: "0".repeat(64), data_url_head: null, href, created_at: T });
  const run = (o) => Object.assign({ id: null, group_id: 0, cell: 0, variant: "", endpoint: "",
    model_asked: "", answered_by: "", fallback_reason: null, upstream_model: null, http_status: 200,
    error_code: null, error_message: null, request: "{}", response: null, response_type: null,
    headers: {}, input_tokens: null, output_tokens: null, latency_ms: 0, created_at: T, artifacts: [] }, o);
  const imageGroups = [
    { id: 9102, lab: "image", title: "a lighthouse on a cliff at dusk, watercolour", form: {},
      plan: [{ model: "image/sdxl-demo", variant: "" }], override: null, created_at: T, artifacts: [],
      item: null, in_flight: false,
      runs: [run({ id: 9202, group_id: 9102, endpoint: "/v1/images/generations", model_asked: "image/sdxl-demo",
        answered_by: "image/sdxl-demo", latency_ms: 18400,
        request: JSON.stringify({ model: "image/sdxl-demo", prompt: "a lighthouse on a cliff at dusk, watercolour", n: 2, size: "1024x1024" }),
        artifacts: [art(9301, 9202, "image-sdxl-demo-1.png", svg("#3b6ea8", "#e0a65a", "1 of 2"), 734003),
          art(9302, 9202, "image-sdxl-demo-2.png", svg("#5a3b8a", "#d97757", "2 of 2"), 801126)] })] },
    { id: 9101, lab: "image", title: "a red cube on a white table", form: {},
      plan: [{ model: "image/sdxl-demo", variant: "" }], override: null, created_at: T, artifacts: [],
      item: null, in_flight: false,
      runs: [run({ id: 9201, group_id: 9101, endpoint: "/v1/images/generations", model_asked: "image/sdxl-demo",
        answered_by: "image/sdxl-demo", http_status: 503, error_code: "gpu_hold",
        error_message: "The GPU is held: local models are paused.", latency_ms: 12,
        request: JSON.stringify({ model: "image/sdxl-demo", prompt: "a red cube on a white table" }),
        response: JSON.stringify({ error: { code: "gpu_hold", message: "The GPU is held: local models are paused." } }),
        response_type: "application/json" })] },
  ];
  const usage = (lab, groups, runs, files, bytes) => ({ lab, groups, batches: 0, runs, artifacts: files,
    artifact_files: files, text_bytes: 2048, artifact_bytes: bytes, orphan_files: 0, orphan_bytes: 0 });

  // --- the Decision lab ----------------------------------------------------------------------
  const decisionModels = {
    hold: false, currency: "USD",
    models: [
      { name: "decide/laya-demo", owner: "lmgw", local: true, model_id: "laya-demo", input_modalities: ["text"],
        context_length: 8192, notes: [], state: "stopped", warnings: [], held: false, fallback: null,
        reads_images: false,
        facts: { header_error: null, decision_type: "choice-head", max_head_tokens: 512, shape: "causal_head",
          ctx_size: 8192, n_ctx_slot: null, ubatch: 512, ubatch_source: "row", batch: 2048,
          batch_source: "llama_cpp_default", backend: "cpu", threads_in_effect: 8, threads_source: "cores" },
        limits: [
          { applies_to: "state_and_question", tokens: 8192, of: "ctx_size", rule: "the state and one question must fit one slot" },
          { applies_to: "option_span", tokens: 512, of: "decision.max_head_tokens", rule: "past it llama.cpp truncates the options" },
        ],
        price: { local: true, rows: [], unresolved: null } },
      { name: "demo-cloud/decide-mini", owner: "demo-cloud", local: false, model_id: null, input_modalities: ["text", "image"],
        context_length: 32768, notes: [], state: null, warnings: [], held: false, fallback: null,
        reads_images: true, facts: null, limits: [], price: { local: false, rows: [], unresolved: null } },
    ],
  };
  const form = {
    state_mode: "text",
    state: "Our whole team cannot load the dashboard at app.example.com, it is down for everyone right now.",
    images: [],
    questions: [
      { id: "team", instructions: "Which team should take this ticket?", kind: { type: "choice", options: [
        { name: "billing", description: "payments, invoices, refunds" },
        { name: "technical", description: "bugs, outages, errors" },
        { name: "account", description: "sign-in, profile, access" },
        { name: "other", description: "" }] } },
      { id: "urgency", instructions: "How urgent is it?", kind: { type: "score", levels: [
        "can wait a week", "this week", "today", "right now"] } },
      { id: "needs_human", instructions: "Does a person have to answer it?", kind: { type: "noul", yes: "a person answers", no: "a canned reply does" } },
    ],
  };
  const answers = (team, p, score, yes) => JSON.stringify({ model: "decide/laya-demo", answers: {
    team: { type: "choice", choice: team, probabilities: p, confidence: 0.62 },
    urgency: { type: "score", score, legend: { 0: "can wait a week", 1: "this week", 2: "today", 3: "right now" },
      probabilities: { 0: 0.02, 1: 0.05, 2: 0.18, 3: 0.75 }, confidence: 0.5 },
    needs_human: { type: "noul", noul: yes, confidence: Math.abs(yes - 0.5) * 2 },
  }, usage: { input_tokens: 212, output_tokens: 0 } });
  const drun = (id, gid, cell, variant, body) => run({ id, group_id: gid, cell, variant, endpoint: "/v1/systemone",
    model_asked: "decide/laya-demo", answered_by: "decide/laya-demo", latency_ms: 140 + cell * 30,
    request: "{}", response: body, input_tokens: 212, output_tokens: 0 });
  const decisionGroups = [
    { id: 9402, lab: "decision", title: form.state, form, override: null, created_at: T, artifacts: [], item: null,
      in_flight: false,
      plan: [{ model: "decide/laya-demo", variant: "as_written" }, { model: "decide/laya-demo", variant: "reversed" }],
      runs: [drun(9502, 9402, 0, "as_written", answers("technical", { technical: 0.81, account: 0.1, billing: 0.06, other: 0.03 }, 2.66, 0.91)),
        drun(9503, 9402, 1, "reversed", answers("technical", { technical: 0.74, account: 0.14, billing: 0.08, other: 0.04 }, 2.58, 0.88))] },
    { id: 9401, lab: "decision", title: "Where can I download the invoice for my March subscription?",
      form: Object.assign({}, form, { state: "Where can I download the invoice for my March subscription?" }),
      override: null, created_at: T, artifacts: [], item: null, in_flight: false,
      plan: [{ model: "decide/laya-demo", variant: "as_written" }],
      runs: [drun(9501, 9401, 0, "as_written", answers("billing", { billing: 0.9, account: 0.05, technical: 0.03, other: 0.02 }, 0.4, 0.12))] },
  ];

  // --- the Voices lab ------------------------------------------------------------------------
  const voiceFacts = (alias) => ({ alias, local: true, row: { model_id: "voice-design-demo", family: "qwen3_tts", variant: "voice_design" },
    description: "voice_design", description_required: true, description_default: false, takes_voice: false,
    default_voice: null, reads_seed: true, row_pins_seed: false, languages: ["en", "de"], language_note: null,
    clones: false, clone_note: null, held: false, fallback: null, licence: null });
  const rounds = { bytes: 3 * 16044, dir: "/home/demo/.local/share/lmgw/voice-lab/rounds", rounds: [
    { id: "r-demo-1", created_at: T, alias: "audio/voice-design-demo", answered_by: "audio/voice-design-demo",
      fallback_reason: null, upstream: "lmgw", upstream_model: "voice-design-demo",
      row: { model_id: "voice-design-demo", family: "qwen3_tts", variant: "voice_design" }, licence: null,
      text: "The North Wind and the Sun were disputing which was the stronger.", sent_text: null, language: "en",
      language_note: null, description: "a warm, calm voice, unhurried", voice: null, seeds_sent: true,
      shaping: null, asked: 3, stopped: null,
      candidates: [1, 2, 3].map((i) => ({ index: i - 1, seed: 4100 + i, duration_ms: 1000, bytes: 16044, mime: "audio/wav", url: wav })) },
  ] };

  const realFetch = window.fetch;
  window.fetch = function (input, init) {
    const url = typeof input === "string" ? input : (input && input.url) || String(input);
    const u = new URL(url, location.href);
    const path = u.pathname;
    if (want.image && path === "/labs/api/image/groups") {
      count("image_groups");
      return Promise.resolve(json({ groups: imageGroups, total: imageGroups.length, older: 0 }));
    }
    if (want.image && path === "/labs/api/image/usage") {
      return Promise.resolve(json(usage("image", 2, 2, 2, 1535129)));
    }
    if (want.decision && path === "/decision-lab/api/models") {
      count("decision_models");
      return Promise.resolve(json(decisionModels));
    }
    if (want.decision && path === "/labs/api/decision/groups") {
      count("decision_groups");
      return Promise.resolve(json({ groups: decisionGroups, total: decisionGroups.length, older: 0 }));
    }
    if (want.decision && path === "/labs/api/decision/usage") {
      return Promise.resolve(json(usage("decision", 2, 3, 0, 0)));
    }
    if (want.voices && path === "/voice-lab/api/model") {
      count("voice_model");
      return Promise.resolve(json(voiceFacts(u.searchParams.get("alias") || "audio/voice-design-demo")));
    }
    if (want.voices && path === "/voice-lab/api/rounds") {
      count("voice_rounds");
      return Promise.resolve(json(rounds));
    }
    return realFetch.apply(this, arguments);
  };
})();
