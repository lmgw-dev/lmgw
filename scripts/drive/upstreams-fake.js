// In-page fakes for the Upstreams page's states a dev copy cannot reach without a real provider
// (scripts/drive/upstreams-split.json): GET /api/upstreams goes to the dev instance, and its
// answer is then given what lmgw remembers on a real gateway — an upstream whose name ends in
// "quota-demo" is out of quota (openai egress design §14.6), and a llama_cpp row whose server
// was never asked has its GET /props facts, a router with one model and one that is unknown
// (llama egress design §4.2). Every other request goes to the dev instance as it is.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/upstreams-fake.js`, before the
// page's own scripts, in every document. On only while sessionStorage "lmgw.fake.upstreams" is
// set. window.__fakeUpstreams counts the answers it changed. The data is made up and looks it.
(() => {
  "use strict";
  if (!sessionStorage.getItem("lmgw.fake.upstreams")) return;
  const real = window.fetch.bind(window);
  window.__fakeUpstreams = 0;
  const now = () => Math.floor(Date.now() / 1000);
  const patch = (body) => {
    for (const u of body.upstreams || []) {
      if (u.name.endsWith("quota-demo") && !u.quota) {
        u.quota = {
          since: now() - 1800,
          code: "insufficient_quota",
          message: "You exceeded your current quota (demo).",
          next_check: now() + 3600,
        };
      }
      if (u.protocol === "llama_cpp" && !(u.llama_facts || []).length) {
        u.llama_facts = [
          { model: "", base_url: u.base_url, router: true, cached: true, read_at: now() - 300,
            probing: false, unknown: null,
            props: { build_info: "b9999-demo", n_ctx_slot: null, vision: null, audio: null, video: null } },
          { model: "demo-chat-7b", base_url: u.base_url, router: false, cached: true,
            read_at: now() - 120, probing: false, unknown: null,
            props: { build_info: "b9999-demo", n_ctx_slot: 32768, vision: true, audio: false, video: false } },
          { model: "demo-embed", base_url: u.base_url, router: false, cached: false, read_at: null,
            probing: false, unknown: "the router has not loaded this model yet (demo)", props: null },
        ];
      }
    }
    return body;
  };
  window.fetch = async function (input, init) {
    const url = typeof input === "string" ? input : input && input.url;
    const method = ((init && init.method) || (input && input.method) || "GET").toUpperCase();
    const res = await real(input, init);
    if (method !== "GET" || !url || new URL(url, location.href).pathname !== "/api/upstreams" || !res.ok) {
      return res;
    }
    const body = patch(await res.clone().json());
    window.__fakeUpstreams += 1;
    return new Response(JSON.stringify(body), {
      status: res.status,
      headers: { "content-type": "application/json" },
    });
  };
})();
