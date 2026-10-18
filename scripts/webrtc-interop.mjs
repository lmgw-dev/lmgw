// Runs the three pages of scripts/webrtc-interop-pages.js in headless
// Chromium, inside the Playwright image (scripts/webrtc-interop.sh starts
// it; no npm packages of its own: Node's fetch and WebSocket speak the
// DevTools protocol).
//
// Usage: node webrtc-interop.mjs BASE WORKDIR
//   WORKDIR holds pages.js, speech.wav (the fake microphone), key and
//   holder.key (two owner keys, removed by the script at the end: the
//   pages' and the bound page's holder's), setup.json (the folder) and
//   npm/ (the Agents SDK, installed by the script). Written there:
//   result.json, and rec-<page>.webm (the remote track, for listening).
//
// Chromium runs as §13.2 has it: fake capture from the WAV
// (--use-fake-device-for-media-stream, --use-file-for-fake-audio-capture)
// granted without a prompt, muted, autoplay allowed, and
// --allow-loopback-in-peer-connection, since the call's one candidate is
// the dev instance's loopback address. Its profile lives in the
// container's /tmp and is removed at the end. Each page runs on a fresh
// navigation to BASE/v1/models: the dev instance's own origin, so the POST
// is same-origin. INTEROP_CONSOLE=1 prints the page's console.
import { spawn } from "node:child_process";
import fs from "node:fs";

const [base, work] = process.argv.slice(2);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const key = fs.readFileSync(`${work}/key`, "utf8").trim();
const holderKey = fs.readFileSync(`${work}/holder.key`, "utf8").trim();
const setup = JSON.parse(fs.readFileSync(`${work}/setup.json`, "utf8"));
const pages = fs.readFileSync(`${work}/pages.js`, "utf8");
const sdkPath = `${work}/npm/node_modules/@openai/agents-realtime/dist/bundle/openai-realtime-agents.umd.js`;
const dir = fs.readdirSync("/ms-playwright").find((d) => /^chromium-\d+$/.test(d));
const chrome = `/ms-playwright/${dir}/chrome-linux64/chrome`;

const result = { checks: [], pages: {}, info: {} };
// `known`: the calls design's question a known finding is tagged with (Q5).
const check = (name, ok, detail, known) => result.checks.push({
  name, ok: !!ok, detail: detail === undefined ? null : detail, ...(known ? { known } : {}),
});

const profile = fs.mkdtempSync("/tmp/lmgw-webrtc-interop-");
const proc = spawn(chrome, [
  "--headless=new", "--no-sandbox", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
  "--remote-debugging-port=9335", `--user-data-dir=${profile}`,
  "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
  `--use-file-for-fake-audio-capture=${work}/speech.wav`, "--mute-audio",
  "--autoplay-policy=no-user-gesture-required", "--allow-loopback-in-peer-connection",
  "about:blank",
], { stdio: ["ignore", "ignore", "ignore"] });

async function close() {
  proc.kill("SIGTERM");
  await new Promise((r) => (proc.exitCode !== null ? r() : proc.once("exit", r)));
  for (let i = 0; i < 5; i++) {
    try {
      fs.rmSync(profile, { recursive: true, force: true });
      break;
    } catch (_) {
      await sleep(200);
    }
  }
}

try {
  let page;
  for (let i = 0; i < 150 && !page; i++) {
    try {
      const r = await fetch("http://127.0.0.1:9335/json/list");
      if (r.ok) page = (await r.json()).find((t) => t.type === "page");
    } catch (_) { /* not up yet */ }
    if (!page) await sleep(100);
  }
  if (!page) throw new Error("Chromium's DevTools never answered");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let next = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && pending.has(d.id)) {
      pending.get(d.id)(d);
      pending.delete(d.id);
    } else if (d.method === "Runtime.consoleAPICalled" && process.env.INTEROP_CONSOLE) {
      console.log("console:", d.params.args.map((a) => a.value ?? a.description ?? "").join(" ").slice(0, 400));
    } else if (d.method === "Runtime.exceptionThrown" && process.env.INTEROP_CONSOLE) {
      console.log("exception:", JSON.stringify(d.params.exceptionDetails).slice(0, 400));
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
      const ed = r.result.exceptionDetails;
      throw new Error((ed.exception && ed.exception.description) || ed.text);
    }
    return r.result && r.result.result ? r.result.result.value : undefined;
  };
  await call("Page.enable");
  await call("Runtime.enable");
  result.info.user_agent = await evaluate("navigator.userAgent");
  const cfg = {
    base, key, holderKey, folder: setup.folder, maxMessage: setup.max_message, stopAfterMs: 1800, pttMs: 2500,
  };
  for (const name of ["stock", "agents", "bound"]) {
    await call("Page.navigate", { url: `${base}/v1/models` });
    await sleep(800);
    try {
      if (name === "agents") {
        if (!fs.existsSync(sdkPath)) throw new Error(`the Agents SDK is not installed at ${sdkPath}`);
        await evaluate(fs.readFileSync(sdkPath, "utf8"));
      }
      await evaluate(pages);
      const started = Date.now();
      const out = await evaluate(`window.lmgwInterop.${name}(${JSON.stringify(cfg)})`);
      if (out.recording) {
        fs.writeFileSync(`${work}/rec-${name}.webm`, Buffer.from(out.recording, "base64"));
        delete out.recording;
      }
      out.seconds = Math.round((Date.now() - started) / 1000);
      result.pages[name] = out;
      for (const c of out.checks) check(`${name}: ${c.name}`, c.ok, c.detail, c.known);
    } catch (e) {
      check(`${name}: the page ran`, false, String(e).slice(0, 600));
    }
    console.log(`page ${name}: ${(result.pages[name] && result.pages[name].checks || []).filter((c) => c.ok).length}`
      + ` of ${(result.pages[name] && result.pages[name].checks || []).length} checks passed`);
    await sleep(1500);
  }
  ws.close();
} catch (e) {
  check("Chromium ran the pages", false, String(e));
} finally {
  await close();
  fs.writeFileSync(`${work}/result.json`, JSON.stringify(result, null, 1));
}
process.exit(result.checks.every((c) => c.ok) ? 0 : 2);
