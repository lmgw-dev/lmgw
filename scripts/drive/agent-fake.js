// In-page fakes for the Agents shots (modern design WP6): a batch agent with a finished run to
// review and an agent whose tools do not resolve, without starting a run, a container or a model.
// It answers, in the page:
//   GET /api/agents                     the instance's own list, with the first chat agent's
//                                       tools reported missing (a "Tools missing" row) and the
//                                       first batch agent's last run set to the fake one;
//   GET /api/agents/<chat agent>        its detail, with the same tool gap;
//   GET /api/agents/<batch agent>/runs  one finished classify run, #9001;
//   GET /api/agents/runs/9001           that run: four rows of example.com mail, one needing
//                                       attention, the batch agent's own review shape.
// Everything else goes to the dev instance as it is. Nothing here writes.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/agent-fake.js` (or ui-matrix.py),
// before the page's own scripts. On while sessionStorage's "lmgw.fake.agents" is "1";
// `?fake-agents=1` on any URL sets it. window.__agentFakeAnswered counts the answers by key.
//
// The data is made up and looks it: senders are @example.com, subjects say "demo".
(() => {
  "use strict";
  try {
    if (new URL(location.href).searchParams.get("fake-agents") === "1") {
      sessionStorage.setItem("lmgw.fake.agents", "1");
    }
  } catch (_) {}
  if (sessionStorage.getItem("lmgw.fake.agents") !== "1") return;
  const answered = (window.__agentFakeAnswered = {});
  const count = (k) => { answered[k] = (answered[k] || 0) + 1; };
  const json = (v) => new Response(JSON.stringify(v), {
    status: 200, headers: { "content-type": "application/json" },
  });
  const T0 = "2026-01-01 09:00:00", T1 = "2026-01-01 09:02:41";
  const JOB = 9001;
  const summary = (agent) => ({
    job_id: JOB, agent_id: agent, phase: "classify", status: "done", done: 4, total: 4,
    percent: 100, stage: "", detail: {}, tokens: 5120, cost_micro: 0, model_calls: 4,
    duration_ms: 161000, error: null, created_at: T0, started_at: T0, finished_at: T1,
  });
  const gap = (r) => ({ label: r.label || "demo-tools", registered: false, missing_tools: [],
    install: null });
  const rows = (cats) => [
    ["demo-1", "2026-01-01", "billing@shop.example.com", "Your demo invoice", cats[0], false],
    ["demo-2", "2026-01-01", "news@list.example.com", "Demo newsletter, issue 12", cats[1], false],
    ["demo-3", "2026-01-01", "friend@example.com", "Demo weekend plans", cats[2], false],
    ["demo-4", "2026-01-01", "unknown@example.com", "A demo message the model was unsure of",
      "Other", true],
  ].map(([id, date, from, subject, category, attention]) => ({
    id, columns: { date, from, subject }, output: { category }, attention,
    raw: attention ? "{\"category\": \"Other\"}" : null,
    prompt: `From: ${from}\nSubject: ${subject}\n\n(demo body)`,
  }));

  let chatId = null, batchId = null, shape = null;
  const orig = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const url = new URL(typeof input === "string" ? input : input.url, location.href);
    const method = ((init && init.method) || (input && input.method) || "GET").toUpperCase();
    const p = url.pathname;
    if (method !== "GET") return orig(input, init);
    if (p === "/api/agents") {
      const list = await (await orig(input, init)).json();
      const chat = list.find((a) => a.kind === "chat");
      const batch = list.find((a) => a.kind === "batch");
      if (chat) {
        chatId = chat.id;
        chat.requires_ok = false;
        chat.requires = (chat.labels.length ? chat.labels : ["demo-tools"]).map((l) => gap({ label: l }));
      }
      if (batch) {
        batchId = batch.id;
        batch.last_run = summary(batch.id);
      }
      count("list");
      return json(list);
    }
    const m = p.match(/^\/api\/agents\/([^/]+)$/);
    if (m && m[1] !== "runs") {
      const d = await (await orig(input, init)).json();
      if (d.kind === "chat") {
        d.requires_ok = false;
        d.requires = (d.requires.length ? d.requires : [{ label: "demo-tools" }]).map(gap);
        count("chat-detail");
      }
      if (d.kind === "batch") {
        batchId = d.id;
        shape = d.batch;
        d.last_run = summary(d.id);
        count("batch-detail");
      }
      return json(d);
    }
    const r = p.match(/^\/api\/agents\/([^/]+)\/runs$/);
    if (r && r[1] === batchId) {
      count("runs");
      return json([summary(batchId)]);
    }
    if (p === `/api/agents/runs/${JOB}` && shape) {
      count("run");
      const cats = (shape.editable[0] && shape.editable[0].options) || ["A", "B", "C"];
      return json({ job: summary(batchId), rows: rows(cats), result: null, batch: shape, log: [] });
    }
    return orig(input, init);
  };
})();
