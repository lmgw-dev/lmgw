// Runs scripts/media-probe.js in headless Chromium, inside the Playwright
// image (scripts/media-probe-chrome.sh starts it; no npm packages: Node's own
// fetch and WebSocket speak the DevTools protocol).
//
// Usage: node media-probe-chrome.mjs BASE WORKDIR
//   WORKDIR holds media-probe.js, speech.wav (TTS-generated), token; the
//   result is written to WORKDIR/result.json.
//
// Chromium gets fake capture from the WAV (--use-fake-device-for-media-stream,
// --use-file-for-fake-audio-capture), grants it without a prompt
// (--use-fake-ui-for-media-stream) and plays nothing (--mute-audio; the
// container has no audio server either). Two launches, each with a profile
// of its own, removed at the end:
// 1. autoplay allowed (its clicks are script clicks): the probe's main run;
//    the microphone permission taken back over CDP while Test microphone
//    runs (the capture must end and say so); a reload, and the probe's
//    reopen phase (the stored input in a fresh document).
// 2. Chromium's default autoplay rule (a user gesture first): the test tone
//    clicked from script must fail visibly ("did not let lmgw's playback
//    start"), then a trusted click (CDP Input.dispatchMouseEvent, a real user
//    gesture) must play it, played == pushed — the player is made inside the
//    press's gesture.
// PROBE_CONSOLE=1 prints the page's console as it runs.
import { spawn } from "node:child_process";
import fs from "node:fs";

const [base, work] = process.argv.slice(2);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const token = fs.readFileSync(`${work}/token`, "utf8").trim();
const dir = fs.readdirSync("/ms-playwright").find((d) => /^chromium-\d+$/.test(d));
const chrome = `/ms-playwright/${dir}/chrome-linux64/chrome`;
const probe = fs.readFileSync(`${work}/media-probe.js`, "utf8");

const result = { checks: [], info: {} };
const check = (name, ok, detail) => result.checks.push({ name, ok: !!ok, detail: detail === undefined ? null : detail });

// One Chromium with its own profile and DevTools port, logged in on /chat
// with the probe loaded.
async function session(port, autoplay) {
  const profile = fs.mkdtempSync("/tmp/lmgw-media-probe-");
  const args = [
    "--headless=new", "--no-sandbox", "--disable-gpu", "--no-first-run",
    "--no-default-browser-check", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
    "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
    `--use-file-for-fake-audio-capture=${work}/speech.wav`, "--mute-audio",
    "--window-size=1440,900", "about:blank",
  ];
  if (autoplay) args.splice(args.length - 2, 0, "--autoplay-policy=no-user-gesture-required");
  const proc = spawn(chrome, args, { stdio: ["ignore", "ignore", "ignore"] });
  const panics = [];
  let ws = null;
  const close = async () => {
    try { ws && ws.close(); } catch (_) { /* gone */ }
    proc.kill("SIGTERM");
    await new Promise((r) => { if (proc.exitCode !== null) r(); else proc.once("exit", r); });
    // The network service may write into the profile just after the browser
    // exits: try a few times.
    for (let i = 0; i < 5; i++) {
      try {
        fs.rmSync(profile, { recursive: true, force: true });
        break;
      } catch (_) {
        await sleep(200);
      }
    }
  };
  try {
    let page;
    for (let i = 0; i < 150 && !page; i++) {
      try {
        const r = await fetch(`http://127.0.0.1:${port}/json/list`);
        if (r.ok) page = (await r.json()).find((t) => t.type === "page");
      } catch (_) { /* not up yet */ }
      if (!page) await sleep(100);
    }
    if (!page) throw new Error("Chromium's DevTools never answered");
    ws = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
    let next = 0;
    const pending = new Map();
    ws.onmessage = (m) => {
      const d = JSON.parse(m.data);
      if (d.id && pending.has(d.id)) {
        pending.get(d.id)(d);
        pending.delete(d.id);
      } else if (d.method === "Runtime.consoleAPICalled" && process.env.PROBE_CONSOLE) {
        console.log("console:", d.params.args.map((a) => a.value ?? a.description ?? "").join(" ").slice(0, 300));
      } else if (d.method === "Runtime.consoleAPICalled" && d.params.type === "error") {
        const text = d.params.args.map((a) => a.value ?? a.description ?? "").join(" ");
        if (/panicked at|RuntimeError: unreachable|already been disposed/.test(text)) panics.push(text);
      } else if (d.method === "Runtime.exceptionThrown") {
        const ed = d.params.exceptionDetails;
        const text = (ed.exception && ed.exception.description) || ed.text || "";
        if (/panicked at|RuntimeError: unreachable/.test(text)) panics.push(text);
      }
    };
    const call = (method, params = {}) => new Promise((res) => {
      const id = ++next;
      pending.set(id, res);
      ws.send(JSON.stringify({ id, method, params }));
    });
    const evaluate = async (expression) => {
      const r = await call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (r.result && r.result.exceptionDetails) {
        throw new Error(r.result.exceptionDetails.exception?.description || r.result.exceptionDetails.text);
      }
      return r.result && r.result.result ? r.result.result.value : undefined;
    };
    const until = async (expr, ms) => {
      const end = Date.now() + ms;
      while (Date.now() < end) {
        try {
          if (await evaluate(expr)) return true;
        } catch (_) { /* navigating */ }
        await sleep(250);
      }
      return false;
    };
    const ready = async () => {
      const ok = await until(
        "!!document.querySelector('main.content') && [...document.querySelectorAll('button')]"
        + ".some(b => (b.title || '').startsWith('New temporary chat'))", 30000);
      if (ok) await evaluate(probe);
      return ok;
    };
    await call("Page.enable");
    await call("Runtime.enable");
    await call("Page.navigate", { url: `${base}/api/session/login?token=${encodeURIComponent(token)}` });
    await sleep(1500);
    await call("Page.navigate", { url: `${base}/chat` });
    if (!(await ready())) {
      check(`the Chat page loaded (${autoplay ? "autoplay allowed" : "a gesture required"})`, false,
        await evaluate("location.href + ' ' + document.body.innerText.slice(0, 200)"));
      await close();
      return null;
    }
    return { call, evaluate, until, ready, panics, close };
  } catch (e) {
    await close();
    throw e;
  }
}

const json = async (s, expr) => JSON.parse(await s.evaluate(expr));

try {
  // --- 1. autoplay allowed: the probe, a revoked permission, the reopen --------------------
  const s = await session(9333, true);
  if (s) {
    result.info.user_agent = await s.evaluate("navigator.userAgent");
    const out = await json(s, `window.lmgwMediaProbe(${JSON.stringify({ inputLabel: "Fake", outputKind: "sinkid" })})`);
    result.checks.push(...out.checks);
    Object.assign(result.info, out.info);

    // The microphone permission taken back while Test microphone runs.
    const stored = await s.evaluate("localStorage.getItem('lmgw.voice.input')");
    await s.evaluate("localStorage.removeItem('lmgw.voice.input')");
    const started = await json(s, "window.lmgwRevokeProbe('start')");
    const origin = new URL(base).origin;
    const set = await s.call("Browser.setPermission", { permission: { name: "microphone" }, setting: "denied", origin });
    const ended = await json(s, "window.lmgwRevokeProbe('check')");
    check("a microphone permission taken back ends the capture: released and said",
      started.mic === "open" && ended.mic === "ended" && ended.notes.some((n) => /permission|stopped delivering/.test(n)),
      { started, ended, setPermission: set.error || "ok" });
    result.info.revoked = ended.notes;
    await s.call("Browser.setPermission", { permission: { name: "microphone" }, setting: "granted", origin });
    if (stored) await s.evaluate(`localStorage.setItem('lmgw.voice.input', ${JSON.stringify(stored)})`);

    // A fresh document with the stored input.
    await s.call("Page.reload", {});
    await sleep(500);
    if (await s.ready()) {
      const again = await json(s, "window.lmgwMediaProbe({phase: 'reopen'})");
      result.checks.push(...again.checks.map((c) => ({ ...c, name: `reopen: ${c.name}` })));
      Object.assign(result.info, again.info);
    } else {
      check("the Chat page loaded again for the reopen phase", false, null);
    }
    // A picture of the popover, for the review.
    await s.evaluate("document.querySelector('[data-voice-devices-btn]')?.click()");
    await sleep(800);
    const shot = await s.call("Page.captureScreenshot", { format: "png" });
    if (shot.result && shot.result.data) {
      fs.writeFileSync(`${work}/chrome-popover.png`, Buffer.from(shot.result.data, "base64"));
    }
    check("no wasm panic in the console (autoplay allowed)", s.panics.length === 0, s.panics);
    await s.close();
  }

  // --- 2. a gesture required: the player is made in the press ------------------------------
  const g = await session(9334, false);
  if (g) {
    await json(g, "window.lmgwGestureProbe('open')");
    await json(g, "window.lmgwGestureProbe('script')");
    let st = {};
    const stop = Date.now() + 15000;
    while (Date.now() < stop) {
      st = await json(g, "window.lmgwGestureProbe('state')");
      if (st.tone === "error" || st.tone === "done") break;
      await sleep(250);
    }
    check("without a user gesture the tone fails visibly: the browser held the playback",
      st.tone === "error" && st.notes.some((n) => n.includes("did not let lmgw's playback start"))
        && st.alert.includes("playback"),
      st);
    // A trusted click: a real user gesture, the context made inside it. The
    // button is measured again: the error note moved it.
    const there = await json(g, "window.lmgwGestureProbe('open')");
    result.info.gesture_click = there;
    const click = async (type) => g.call("Input.dispatchMouseEvent",
      { type, x: there.x, y: there.y, button: "left", clickCount: 1 });
    await click("mouseMoved");
    await click("mousePressed");
    await click("mouseReleased");
    const end = Date.now() + 15000;
    while (Date.now() < end) {
      st = await json(g, "window.lmgwGestureProbe('state')");
      if (st.tone === "error" || st.tone === "done") break;
      await sleep(250);
    }
    check("a click (a user gesture) makes the player and plays the tone, played == pushed",
      st.tone === "done" && st.pushed > 0 && st.pushed === st.played && st.player === "running" && !st.alert,
      st);
    check("no wasm panic in the console (a gesture required)", g.panics.length === 0, g.panics);
    await g.close();
  }
} catch (e) {
  check("the Chromium probe ran to the end", false, String((e && e.stack) || e));
}
fs.writeFileSync(`${work}/result.json`, JSON.stringify(result, null, 1));
let failed = 0;
for (const c of result.checks) {
  if (!c.ok) failed++;
  console.log(`[${c.ok ? "PASS" : "FAIL"}] ${c.name}: ${JSON.stringify(c.detail).slice(0, 300)}`);
}
console.log(`info: ${JSON.stringify(result.info).slice(0, 2500)}`);
console.log(`${result.checks.length - failed}/${result.checks.length} passed`);
process.exit(failed || !result.checks.length ? 2 : 0);
