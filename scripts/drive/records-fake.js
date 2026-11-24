// In-page fakes for the record views (modern design WP10): a request log, stored conversations
// and benchmark runs on a dev copy that has none, without a model, a container, a GPU or a
// cloud request. It answers, in the page:
//   GET  /api/logs                  300 made-up requests over two days (ids 9300 down to 9001),
//                                   filtered the way the server filters them (alias, alias_q,
//                                   upstream, upstream_q, class, errors_only, error_kind,
//                                   before_id, limit); every 7th one failed;
//   GET  /api/responses             three stored conversations, one blocked on an approval;
//   GET  /api/responses/chain?id=   that conversation's turns;
//   POST /api/op/bench_runs         four runs: one done, one partial, one failed, one canceled;
//   POST /api/op/bench_run          that run, with its headline (no phases measured);
//   POST /api/op/bench_plan         a plan that cannot start (the fake has no row to bench);
//   GET  /api/wiring                four local models (one with its file missing, one private
//                                   with no alias, one disabled), two unwired files, one upstream.
// Everything else goes to the dev instance as it is. bench_start, bench_cancel and bench_delete
// are refused here with a 400, so no drive can start or change a run.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/records-fake.js` (or ui-matrix.py),
// before the page's own scripts. On while sessionStorage's "lmgw.fake.records" is "1";
// `?fake-records=1` on any URL sets it. window.__recordsFakeAnswered counts the answers by key,
// and window.__recordsFakeHold (a key → ms map) holds an answer back to test stale answers.
//
// The data is made up and looks it: aliases say "demo", upstreams are .example names.
(() => {
  "use strict";
  try {
    if (new URL(location.href).searchParams.get("fake-records") === "1") {
      sessionStorage.setItem("lmgw.fake.records", "1");
    }
  } catch (_) {}
  if (sessionStorage.getItem("lmgw.fake.records") !== "1") return;
  const answered = (window.__recordsFakeAnswered = {});
  window.__recordsFakeHold = window.__recordsFakeHold || {};
  const count = (k) => { answered[k] = (answered[k] || 0) + 1; };
  const json = (v, status = 200) => new Response(JSON.stringify(v), {
    status, headers: { "content-type": "application/json" },
  });
  const hold = async (k) => {
    const ms = window.__recordsFakeHold[k];
    if (ms) await new Promise((r) => setTimeout(r, ms));
  };
  const pad = (n) => String(n).padStart(2, "0");

  // ---- the request log ----
  const ALIASES = ["demo-chat", "demo-coder-30b", "embed/demo-embed", "demo-vision", "cloud-demo/large"];
  const UPSTREAMS = ["local.example", "local.example", "local.example", "local.example", "api.example"];
  const CLASSES = ["chat", "chat", "aux", "chat", "chat"];
  const rows = [];
  for (let i = 0; i < 300; i++) {
    const id = 9300 - i;
    const k = i % ALIASES.length;
    const failed = i % 7 === 3;
    // Two days: the newest 180 today, the rest yesterday, a minute apart.
    const day = i < 180 ? "2026-01-02" : "2026-01-01";
    const m = (i < 180 ? 720 - i : 1320 - (i - 180)) % 1440;
    const ts = `${day} ${pad(Math.floor(m / 60))}:${pad(m % 60)}:${pad((i * 7) % 60)}`;
    rows.push({
      log_id: id, ts, client_key: i % 3 === 0 ? "demo-key" : null,
      ingress_proto: i % 11 === 5 && k === 0 ? "mcp" : "openai", requested_alias: ALIASES[k],
      upstream_name: UPSTREAMS[k], upstream_model: k === 4 ? "large-demo-2026" : `${ALIASES[k]}.gguf`,
      egress_proto: k === 4 ? "anthropic" : "llama", status: failed ? 503 : 200,
      ttfb_ms: 40 + (i % 13) * 17, total_ms: 400 + (i % 29) * 113,
      prompt_tokens: failed ? null : 120 + (i % 17) * 40, completion_tokens: failed ? null : 30 + (i % 9) * 25,
      cached_in_tokens: k === 4 ? 64 : null, cache_write_tokens: null, streamed: i % 2 === 0,
      error_kind: failed ? "upstream_unavailable" : null,
      error_msg: failed ? "demo: the upstream did not answer within the deadline (made-up error)" : null,
      mcp_tool: i % 11 === 5 && k === 0 ? "demo_tool_search" : null, cost_micro: k === 4 ? 1200 : null,
      class: CLASSES[k], key_id: i % 3 === 0 ? 1 : null, max_tokens_clamped: null,
      fallback_reason: i % 23 === 9 ? "hold" : null, degraded: null, rung: null,
    });
  }
  const logs = (url) => {
    const q = url.searchParams;
    const has = (hay, n) => !n || (hay || "").toLowerCase().includes(n.toLowerCase());
    let out = rows.filter((r) =>
      (!q.get("alias") || r.requested_alias === q.get("alias")) &&
      has(r.requested_alias, q.get("alias_q")) &&
      (!q.get("upstream") || r.upstream_name === q.get("upstream")) &&
      (!q.get("upstream_q") || has(r.upstream_name, q.get("upstream_q"))) &&
      (!q.get("class") || r.class === q.get("class")) &&
      (!q.get("error_kind") || r.error_kind === q.get("error_kind")) &&
      (!["1", "true", "yes", "on"].includes(q.get("errors_only") || "") || r.status >= 400) &&
      (!q.get("before_id") || r.log_id < Number(q.get("before_id"))));
    out = out.slice(0, Number(q.get("limit") || 50));
    return { logs: out };
  };

  // ---- stored conversations ----
  const chains = [
    { chain_id: "resp_demo_chain_alpha", head_id: "resp_demo_a3", model: "demo-chat", status: "completed",
      responses: 3, first_at: "2026-01-02 09:00:00", last_at: "2026-01-02 09:12:00",
      input_tokens: 2400, output_tokens: 610, size: "18.2 KiB", awaiting_approval: false },
    { chain_id: "resp_demo_chain_beta", head_id: "resp_demo_b2", model: "cloud-demo/large", status: "in_progress",
      responses: 2, first_at: "2026-01-02 08:00:00", last_at: "2026-01-02 08:30:00",
      input_tokens: 900, output_tokens: 120, size: "6.0 KiB", awaiting_approval: true },
    { chain_id: "resp_demo_chain_gamma", head_id: "resp_demo_c1", model: "demo-coder-30b", status: "failed",
      responses: 1, first_at: "2026-01-01 17:00:00", last_at: "2026-01-01 17:00:30",
      input_tokens: 300, output_tokens: 0, size: "1.1 KiB", awaiting_approval: false },
  ];
  const turn = (id, status, text, pending = []) => ({
    id, created_at: "2026-01-02 09:00:00", status, input_tokens: 800, output_tokens: 200,
    outline: "user text → assistant text (demo)", pending, text,
    body: JSON.stringify({ id, object: "response", status, output: [{ type: "message", content: [{ type: "output_text", text }] }] }, null, 2),
  });
  const chainDetail = (id) => {
    const c = chains.find((c) => c.chain_id === id);
    if (!c) return null;
    const n = c.responses;
    const turns = [];
    for (let i = 1; i <= n; i++) {
      const last = i === n;
      turns.push(turn(`${c.head_id.slice(0, -1)}${i}`, last ? c.status : "completed",
        `Demo answer ${i} of ${c.chain_id}: made-up text that reads like an assistant turn, long enough to wrap onto a second line in a narrow pane.`,
        last && c.awaiting_approval ? ["demo_tool_delete"] : []));
    }
    return { chain_id: id, responses: turns };
  };

  // ---- benchmark runs ----
  const summary = (id, status, extra) => Object.assign({
    id, job_id: null, created_at: "2026-01-02 10:00:00", finished_at: "2026-01-02 10:20:00", status,
    status_reason: null, error: null, complete: true, model_id: "demo-chat", quant: "Q4_K_M", rung: 0,
    image_ref: "localhost/lmgw-llama-server:official-master", engine_slug: "official-master",
    version: "b9999", commit: "0123456789", build_info: null, gpu_name: "Demo GPU", throttled: false,
    suite_version: 1, repetitions: 3, phases: ["load", "probes", "prefill", "decode"],
    headline: { prefill_tok_s: 4210.5, prefill_tokens: 2048, ttft_ms: 486, decode_tok_s: 61.2,
      aggregate_tok_s: 212.4, aggregate_streams: 4, decode_tokens_per_joule: 1.66,
      load_ms: 8200, load_vram_bytes: 6442450944 },
    probes_passed: 5, probes_judged: 6, notes: "", previous_id: null, regressions: null,
    improvements: null, unreadable: [],
  }, extra || {});
  const runs = [
    summary(44, "done", { previous_id: 41, regressions: 1, improvements: 2 }),
    summary(43, "done", { complete: false, model_id: "demo-coder-30b", quant: "Q5_K_M", rung: 1 }),
    summary(42, "failed", { error: "demo: the container did not answer /health (made-up error)",
      model_id: "demo-vision", complete: false }),
    summary(41, "canceled", { status_reason: "canceled", complete: false }),
  ];
  const runDetail = (id, threshold) => {
    const s = runs.find((r) => r.id === id);
    if (!s) return null;
    return {
      run: {
        id: s.id, created_at: s.created_at, finished_at: s.finished_at, status: s.status,
        status_reason: s.status_reason, error: s.error,
        model: { model_id: s.model_id, rung: s.rung, quant: s.quant },
        image_ref: s.image_ref, build: { engine_slug: s.engine_slug, version: s.version, commit: s.commit },
        command_line: "llama-server --model /models/demo.gguf --ctx-size 8192 (demo)",
        suite_version: 1, params: { phases: s.phases, repetitions: 3 },
        results: { complete: s.complete, phases_done: s.complete ? s.phases : ["load"] },
        notes: s.notes, unreadable: [],
      },
      headline: s.headline, comparison: null, threshold_pct: threshold || 5, live_job_id: null,
    };
  };

  // ---- the wiring ----
  const local = (id, model_id, extra) => Object.assign({
    id, model_id, hf_repo: "demo-org/demo-models-GGUF", hf_status: "done",
    gguf_path: `/models/demo-org/${model_id}-Q4_K_M.gguf`, file_exists: true, enabled: true,
    public: true, public_name: model_id, aliases: [],
  }, extra || {});
  const wiring = {
    locals: [
      local(1, "demo-chat", { aliases: ["chat", "default"] }),
      local(2, "demo-coder-30b", { file_exists: false }),
      local(3, "demo-vision", { public: false, aliases: [] }),
      local(4, "demo-old", { enabled: false, hf_repo: "", hf_status: "" }),
    ],
    orphans: [
      { gguf_path: "/models/demo-org/demo-unused-Q8_0.gguf", suggested_id: "demo-unused", role_guess: "weights" },
      { gguf_path: "/models/demo-org/mmproj-demo-vision-f16.gguf", suggested_id: "mmproj-demo-vision", role_guess: "mmproj" },
    ],
    upstreams: [
      { id: 1, name: "api.example", protocol: "openai", enabled: true, expose_all: false,
        prefix: "cloud-demo", aliases: ["cloud-demo/large"] },
    ],
    models_dir: "/models", models_dir_missing: false, base_url: "http://127.0.0.1:8934",
  };

  const orig = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const url = new URL(typeof input === "string" ? input : input.url, location.href);
    const method = ((init && init.method) || (input && input.method) || "GET").toUpperCase();
    const p = url.pathname;
    if (method === "GET" && p === "/api/logs") {
      count("logs");
      await hold("logs");
      return json(logs(url));
    }
    if (method === "GET" && p === "/api/wiring") {
      count("wiring");
      return json(Object.assign({}, wiring, { base_url: location.origin }));
    }
    if (method === "GET" && p === "/api/responses") {
      count("responses");
      return json({ chains, total_chains: chains.length, total_responses: 6, store_enabled: true,
        retention_hours: 720, max_chains: 1000, rules: "kept 30 days, at most 1000 (demo)", capped: false });
    }
    if (method === "GET" && p === "/api/responses/chain") {
      const id = url.searchParams.get("id");
      count(`chain:${id}`);
      await hold(`chain:${id}`);
      const d = chainDetail(id);
      return d ? json(d) : json({ error: { message: "no such conversation" } }, 404);
    }
    if (method === "POST" && p.startsWith("/api/op/bench_")) {
      const op = p.slice("/api/op/".length);
      let body = {};
      try { body = JSON.parse((init && init.body) || "{}"); } catch (_) {}
      count(op);
      if (op === "bench_runs") {
        return json({ runs, more: false, threshold_pct: body.threshold_pct || 5 });
      }
      if (op === "bench_run") {
        await hold(`bench_run:${body.id}`);
        const d = runDetail(body.id, body.threshold_pct);
        return d ? json(d) : json({ error: { message: `no run ${body.id}` } }, 404);
      }
      if (op === "bench_plan") {
        return json({ error: { message: "demo: the fake has no row to benchmark" } }, 400);
      }
      return json({ error: { message: `demo: ${op} is refused by the drive's fake` } }, 400);
    }
    return orig(input, init);
  };
})();
