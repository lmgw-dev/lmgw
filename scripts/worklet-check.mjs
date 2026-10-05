#!/usr/bin/env node
// Usage: node scripts/worklet-check.mjs [--long]
//
// Offline checks of the page's two audio worklets (chat-voice §11), in plain
// Node with a stub AudioWorkletProcessor: no browser, no audio device. Run by
// ci/check.sh when node is installed. --long runs the resampler's rate check
// over 20 minutes of input instead of 3.
//
// crates/lmgw-ui/assets/voice/capture-worklet.js — the resampler and chunker:
//   THD+N at 1 kHz for 48 and 44.1 kHz into 24 and 16 kHz, with 128-, 127- and
//   480-frame blocks (the state carried across process() calls); the
//   frequency response near the top of the band; aliases from above the
//   output's Nyquist frequency, the transition band included; the output
//   count against the ideal over minutes (no drift); 40 ms chunks; the
//   push-to-talk pre-roll contiguous with the live audio; a gate close that
//   posts the utterance's tail before `gated`; flush.
// crates/lmgw-ui/assets/voice/player-worklet.js — the player:
//   gapless chunks, played == pushed, progress about every 50 ms, `underrun`
//   (an item still waits) apart from `drained` (nothing waits), `ended`, a
//   flush's counts, a push after `end` ignored, PCM16 scaling.
//
// It prints the measured figures (the ones chat-voice §11.2 quotes) and one
// PASS/FAIL line per check; exit 1 when one failed.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const voice = path.join(here, "..", "crates", "lmgw-ui", "assets", "voice");
const long = process.argv.includes("--long");

// --- the worklet scope stub ----------------------------------------------------------
const sources = {};
for (const f of ["capture-worklet.js", "player-worklet.js"]) {
  sources[f] = fs.readFileSync(path.join(voice, f), "utf8");
}
let registered = {};
globalThis.registerProcessor = (name, cls) => { registered[name] = cls; };
globalThis.AudioWorkletProcessor = class {
  constructor() {
    this.posted = [];
    this.port = { postMessage: (m) => this.posted.push(m), onmessage: null };
  }
};
function load(file, name, rate, options) {
  globalThis.sampleRate = rate;
  registered = {};
  new Function(sources[file])();
  const p = new registered[name](options);
  p.send = (m) => p.port.onmessage({ data: m });
  return p;
}

// --- checks ----------------------------------------------------------------------
let failed = 0;
function check(name, ok, detail) {
  if (!ok) failed++;
  console.log(`[${ok ? "PASS" : "FAIL"}] ${name}: ${detail}`);
}
const db = (x) => 20 * Math.log10(Math.max(x, 1e-12));
const fmt = (x) => x.toFixed(1);

// Least-squares fit of a·sin + b·cos + c at a known frequency; the amplitude
// and the residual's RMS.
function fit(x, freq, rate) {
  let s = [0, 0, 0, 0, 0, 0, 0, 0, 0];
  let r = [0, 0, 0];
  for (let n = 0; n < x.length; n++) {
    const w = (2 * Math.PI * freq * n) / rate;
    const v = [Math.sin(w), Math.cos(w), 1];
    for (let i = 0; i < 3; i++) {
      r[i] += v[i] * x[n];
      for (let j = 0; j < 3; j++) s[i * 3 + j] += v[i] * v[j];
    }
  }
  const [a, b, c] = solve3(s, r);
  let res = 0;
  for (let n = 0; n < x.length; n++) {
    const w = (2 * Math.PI * freq * n) / rate;
    const e = x[n] - (a * Math.sin(w) + b * Math.cos(w) + c);
    res += e * e;
  }
  return { amp: Math.hypot(a, b), residual: Math.sqrt(res / x.length) };
}
function solve3(m, v) {
  const a = [[m[0], m[1], m[2], v[0]], [m[3], m[4], m[5], v[1]], [m[6], m[7], m[8], v[2]]];
  for (let i = 0; i < 3; i++) {
    let p = i;
    for (let k = i + 1; k < 3; k++) if (Math.abs(a[k][i]) > Math.abs(a[p][i])) p = k;
    [a[i], a[p]] = [a[p], a[i]];
    for (let k = i + 1; k < 3; k++) {
      const f = a[k][i] / a[i][i];
      for (let j = i; j < 4; j++) a[k][j] -= f * a[i][j];
    }
  }
  const x = [0, 0, 0];
  for (let i = 2; i >= 0; i--) {
    let t = a[i][3];
    for (let j = i + 1; j < 3; j++) t -= a[i][j] * x[j];
    x[i] = t / a[i][i];
  }
  return x;
}

// Run the capture worklet over `secs` of a sine; returns the output samples
// (−1…1) and the messages.
function capture(inRate, outRate, signal, secs, block = 128, opts = {}) {
  const p = load("capture-worklet.js", "lmgw-capture", inRate,
    { processorOptions: { targetRate: outRate, chunkMs: 40, ...opts } });
  const total = Math.round(inRate * secs);
  for (let off = 0; off < total; off += block) {
    const n = Math.min(block, total - off);
    const blk = new Float32Array(n);
    for (let i = 0; i < n; i++) blk[i] = signal(off + i, inRate);
    p.process([[blk, blk]]);
  }
  const out = [];
  for (const m of p.posted) if (m.type === "chunk") for (const s of new Int16Array(m.pcm)) out.push(s / 32768);
  return { out, posted: p.posted, p };
}
const sine = (f, a = 0.5) => (n, rate) => a * Math.sin((2 * Math.PI * f * n) / rate);
const steady = (x) => x.slice(Math.floor(x.length * 0.2), Math.floor(x.length * 0.8));

console.log("== capture worklet (resampler, chunks, gate)");
for (const [inRate, outRate] of [[48000, 24000], [44100, 24000], [48000, 16000], [44100, 16000]]) {
  const worst = Math.max(...[128, 127, 480].map((block) => {
    const { out } = capture(inRate, outRate, sine(1000), 1.5, block);
    const f = fit(steady(out), 1000, outRate);
    return db(f.residual / (f.amp / Math.SQRT2));
  }));
  check(`THD+N at 1 kHz, ${inRate / 1000} → ${outRate / 1000} kHz (blocks of 128, 127, 480)`,
    worst < -80, `${fmt(worst)} dB at worst`);
}
{
  const { out } = capture(24000, 24000, sine(1000), 1);
  const f = fit(steady(out), 1000, 24000);
  check("no resampling at an equal rate: the input as it came, quantised", f.residual < 2e-5,
    `residual ${f.residual.toExponential(1)}`);
}
const response = (inRate, outRate, freq) => {
  const { out } = capture(inRate, outRate, sine(freq), 1);
  return db(fit(steady(out), freq, outRate).amp / 0.5);
};
const alias = (inRate, outRate, freq) => {
  const { out } = capture(inRate, outRate, sine(freq), 1);
  return db(fit(steady(out), outRate - freq, outRate).amp / 0.5);
};
{
  const r10 = response(48000, 24000, 10000);
  const r7 = response(48000, 16000, 7000);
  const r1 = response(44100, 16000, 1000);
  console.log(`  response: 1 kHz ${fmt(r1)} dB (44.1 → 16), 10 kHz ${fmt(r10)} dB (48 → 24), 7 kHz ${fmt(r7)} dB (48 → 16)`);
  check("flat at 1 kHz", Math.abs(r1) < 0.1, `${r1.toFixed(2)} dB`);
  check("the band's top: 10 kHz into 24 kHz, 7 kHz into 16 kHz", r10 > -3 && r7 > -4.5,
    `${fmt(r10)} dB, ${fmt(r7)} dB`);
  const cases = [[48000, 24000, 14000], [48000, 24000, 13000], [48000, 24000, 12300], [48000, 24000, 12100],
    [44100, 24000, 13000], [48000, 16000, 9000], [48000, 16000, 8300], [48000, 16000, 8100], [44100, 16000, 8300]];
  const got = cases.map(([i, o, f]) => [i, o, f, alias(i, o, f)]);
  for (const [i, o, f, a] of got) {
    console.log(`  alias: ${f / 1000} kHz at ${i / 1000} → ${o / 1000} kHz lands on ${(o - f) / 1000} kHz at ${fmt(a)} dB`);
  }
  const worst = Math.max(...got.map((g) => g[3]));
  check("aliases from above the output's Nyquist frequency, transition band included", worst < -40,
    `${fmt(worst)} dB at worst`);
}
{
  const secs = long ? 1200 : 180;
  const inRate = 44100;
  const outRate = 16000;
  const p = load("capture-worklet.js", "lmgw-capture", inRate, { processorOptions: { targetRate: outRate } });
  const blk = new Float32Array(128);
  let n = 0;
  const total = inRate * secs;
  for (let off = 0; off < total; off += 128) {
    p.process([[blk]]);
    if (p.posted.length > 1000) {
      for (const m of p.posted) if (m.type === "chunk") n += m.pcm.byteLength / 2;
      p.posted.length = 0;
    }
  }
  for (const m of p.posted) if (m.type === "chunk") n += m.pcm.byteLength / 2;
  // Output n is made once the input reaches n·ratio plus the filter's half
  // length: exactly this many after `fed` input frames.
  const fed = Math.ceil(total / 128) * 128;
  const ideal = Math.ceil((fed - p.half) / p.ratio);
  const off = n + p.fill - ideal;
  check(`no drift: ${secs / 60} min of 44.1 → 16 kHz`, Math.abs(off) <= 1,
    `${n + p.fill} samples out of ${fed} in, ${off} from the ideal ${ideal}`);
}
{
  const { posted } = capture(48000, 24000, sine(440), 1);
  const lens = new Set(posted.filter((m) => m.type === "chunk").map((m) => m.pcm.byteLength / 2));
  const { posted: p16 } = capture(44100, 16000, sine(440), 1);
  const l16 = new Set(p16.filter((m) => m.type === "chunk").map((m) => m.pcm.byteLength / 2));
  check("40 ms chunks: 960 samples at 24 kHz, 640 at 16 kHz", lens.size === 1 && lens.has(960) && l16.size === 1 && l16.has(640),
    `${[...lens]} / ${[...l16]}`);
}
{
  // Push-to-talk against an ungated run of the same input: the pre-roll and
  // the live audio are one contiguous stretch, and closing the gate posts the
  // tail up to the last sample made.
  const sig = (n, rate) => 0.3 * Math.sin((2 * Math.PI * 523 * n) / rate) + 0.2 * Math.sin((2 * Math.PI * 97 * n) / rate);
  const inRate = 48000;
  const ref = capture(inRate, 24000, sig, 2).out;
  const p = load("capture-worklet.js", "lmgw-capture", inRate,
    { processorOptions: { targetRate: 24000, chunkMs: 40, prerollMs: 200, gated: true } });
  let made = 0; // input frames fed
  const feed = (frames) => {
    for (let k = 0; k < frames; k += 128) {
      const blk = new Float32Array(128);
      for (let i = 0; i < 128; i++) blk[i] = sig(made + i, inRate);
      made += 128;
      p.process([[blk]]);
    }
  };
  feed(48000 * 0.6);
  const whileGated = p.posted.length;
  p.send({ type: "gate", open: true });
  const preroll = p.posted.filter((m) => m.type === "chunk").length;
  feed(48000 * 0.5 + 128 * 7); // ends mid-chunk
  p.send({ type: "gate", open: false });
  const msgs = p.posted.map((m) => m.type);
  const chunks = p.posted.filter((m) => m.type === "chunk");
  const got = [];
  for (const m of chunks) for (const s of new Int16Array(m.pcm)) got.push(s / 32768);
  // Chunks are cut from the first output sample on, so the pre-roll starts
  // on a chunk boundary of the reference.
  let at = -1;
  for (let j = 0; j + 50 <= ref.length; j += 960) {
    if (got.slice(0, 50).every((v, k) => v === ref[j + k])) { at = j; break; }
  }
  let mismatch = at < 0 ? got.length : 0;
  if (at >= 0) for (let k = 0; k < got.length; k++) if (got[k] !== ref[at + k]) mismatch++;
  const tail = chunks[chunks.length - 1].pcm.byteLength / 2;
  check("gated, nothing is sent", whileGated === 0, `${whileGated} messages`);
  check("opening the gate sends the 200 ms pre-roll at once", preroll === 5, `${preroll} chunks`);
  check("the pre-roll, the live audio and the tail are one contiguous stretch of the input",
    at >= 0 && mismatch === 0, `${got.length} samples from output sample ${at}, ${mismatch} differ`);
  check("closing the gate posts the partial chunk, then says gated",
    msgs[msgs.length - 1] === "gated" && msgs[msgs.length - 2] === "chunk" && tail > 0 && tail < 960,
    `last chunk ${tail} samples, then ${msgs[msgs.length - 1]}`);
  // An ungated run fed the same frames, then flushed, made exactly this many.
  const twin = load("capture-worklet.js", "lmgw-capture", inRate, { processorOptions: { targetRate: 24000, chunkMs: 40 } });
  for (let f = 0; f < made; f += 128) {
    const blk = new Float32Array(128);
    for (let i = 0; i < 128; i++) blk[i] = sig(f + i, inRate);
    twin.process([[blk]]);
  }
  twin.port.onmessage({ data: { type: "flush" } });
  let all = 0;
  for (const m of twin.posted) if (m.type === "chunk") all += m.pcm.byteLength / 2;
  check("nothing of the utterance is held back at the close", got.length === all - at,
    `${got.length} posted, ${all - at} made since the pre-roll's start`);
  p.posted.length = 0;
  feed(48000 * 0.3);
  check("closed again, it sends nothing", p.posted.length === 0, `${p.posted.length} messages`);
  p.send({ type: "flush" });
  check("a flush while gated keeps the pre-roll's partial chunk", p.posted.length === 1 && p.posted[0].type === "flushed",
    p.posted.map((m) => m.type).join(","));
  p.send({ type: "gate", open: true });
  p.posted.length = 0;
  feed(128 * 3);
  p.send({ type: "flush" });
  const last = p.posted.filter((m) => m.type === "chunk").pop();
  check("a flush while open posts the partial chunk, then says flushed",
    p.posted[p.posted.length - 1].type === "flushed" && !!last && last.pcm.byteLength > 0,
    p.posted.map((m) => m.type).join(","));
  p.send({ type: "stop" });
  check("stop ends processing", p.process([[new Float32Array(128)]]) === false, "process() answers false");
}

console.log("== player worklet");
{
  const p = load("player-worklet.js", "lmgw-player", 24000);
  const mk = (n, v) => Int16Array.from({ length: n }, (_, i) => v + (i % 7)).buffer;
  const render = (blocks) => {
    const out = [];
    for (let q = 0; q < blocks; q++) {
      const o = [new Float32Array(128)];
      p.process([], [o]);
      out.push(...o[0]);
    }
    return out;
  };
  p.send({ type: "push", item: 1, pcm: mk(1000, 1000) });
  p.send({ type: "push", item: 1, pcm: mk(1500, 2000) });
  let out = render(25);
  const expect = [...new Int16Array(mk(1000, 1000)), ...new Int16Array(mk(1500, 2000))].map((s) => s / 32768);
  check("chunks play back to back, gapless, as PCM16 / 32768",
    expect.every((v, i) => out[i] === v) && out.slice(2500).every((v) => v === 0),
    `${expect.length} samples, then silence`);
  let types = p.posted.map((m) => m.type);
  check("an item not ended that runs dry is an underrun, not a drain",
    types.includes("underrun") && !types.includes("drained") && !types.includes("ended"), types.filter((t) => t !== "progress").join(","));
  const under = p.posted.find((m) => m.type === "underrun");
  check("the underrun names the waiting item and its counts",
    under.items.length === 1 && under.items[0].item === 1 && under.items[0].played === 2500, JSON.stringify(under.items));
  check("progress about every 50 ms", p.posted.filter((m) => m.type === "progress").length === 2,
    `${p.posted.filter((m) => m.type === "progress").length} reports over 2500 samples`);
  p.posted.length = 0;
  p.send({ type: "end", item: 1 });
  types = p.posted.map((m) => m.type);
  check("its end, after the underrun: ended, then drained", types.join(",") === "ended,drained", types.join(","));
  const ended = p.posted[0];
  check("ended: played == pushed", ended.played === 2500 && ended.pushed === 2500, JSON.stringify(ended));

  p.posted.length = 0;
  p.send({ type: "push", item: 2, pcm: mk(600, 10) });
  p.send({ type: "end", item: 2 });
  p.send({ type: "push", item: 2, pcm: mk(600, 10) });
  render(8);
  types = p.posted.filter((m) => m.type !== "progress").map((m) => m.type);
  check("an ended item that plays out: ended, then drained, no underrun", types.join(",") === "ended,drained", types.join(","));
  check("a push after end is ignored", p.posted.find((m) => m.type === "ended").pushed === 600,
    JSON.stringify(p.posted.find((m) => m.type === "ended")));

  p.posted.length = 0;
  p.send({ type: "push", item: 3, pcm: mk(24000, 5) });
  render(10);
  p.send({ type: "flush", id: 7, item: 3 });
  const fl = p.posted.find((m) => m.type === "flushed");
  check("a flush answers what was played of the item", fl && fl.id === 7 && fl.items.length === 1
    && fl.items[0].pushed === 24000 && fl.items[0].played === 1280, JSON.stringify(fl));
  p.posted.length = 0;
  out = render(2);
  check("after the flush the queue is empty: silence, and a drain",
    out.every((v) => v === 0) && p.posted.map((m) => m.type).join(",") === "drained", p.posted.map((m) => m.type).join(","));
  p.posted.length = 0;
  p.send({ type: "flush", id: 8, item: 2 });
  check("flushing an item that already ended answers nothing for it (the page holds its count)",
    p.posted[0].type === "flushed" && p.posted[0].items.length === 0, JSON.stringify(p.posted[0]));

  p.posted.length = 0;
  p.send({ type: "push", item: 4, pcm: mk(300, 5) });
  render(3);
  p.send({ type: "flush", id: 9 });
  types = p.posted.filter((m) => m.type !== "progress").map((m) => m.type);
  check("flushing an item waiting after an underrun ends the dry spell with a drain",
    types.join(",") === "underrun,flushed,drained", types.join(","));
}

console.log(failed ? `${failed} check(s) failed` : "all checks passed");
process.exit(failed ? 1 : 0);
