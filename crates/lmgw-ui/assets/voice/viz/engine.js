// The realtime panel's visualisation engine (chat-voice §10), shared by the
// three variants (ribbon.js, orb.js, ring.js). Ported from the owner's
// verified sample (its §1–§5: constants, the analyser taps, the five-state
// machine with eased crossfades, the per-frame object every renderer reads)
// without changing a number: the look is the sample's.
//
// A variant module is `export function mount(canvas, inputs)`, which calls
// `run(canvas, inputs, factory)` here. `factory(canvas, opts)` makes the
// renderer: `{kind, resize(w, h, dpr), render(F), lost?, release?()}`.
//
// The handle (§10): setState(state, info), setTiming(timing), resize(cssW,
// cssH, dpr), destroy() — and setInputs({output, input}) for a microphone
// opened (or closed) after the mount, and stats() for the probes.
//
// The engine owns its requestAnimationFrame loop, reads the analysers itself,
// and pauses while the document is hidden or the canvas is out of view. Idle
// with both taps quiet (for QUIET_S), and in reduced motion, it draws at
// IDLE_FPS instead of the display's rate: a long idle session costs the GPU
// and the compositor little (WP11 UI review NIT 8). Any state change, a
// voice on either tap, or a resize brings the full rate back at once.

export const BANDS = 48;
export const S = { IDLE: 0, LISTENING: 1, THINKING: 2, SPEAKING: 3, INTERRUPTED: 4 };
export const STATE_NAMES = ["idle", "listening", "thinking", "speaking", "interrupted"];
export const TAU = Math.PI * 2;

export const clamp01 = (x) => (x < 0 ? 0 : x > 1 ? 1 : x);
export const sq = (x) => x * x;
export const smoothstep = (a, b, x) => {
  const t = clamp01((x - a) / (b - a));
  return t * t * (3 - 2 * t);
};

function hexRgb(h) {
  const s = String(h || "").trim().replace(/^#/, "");
  const full = s.length === 3 ? s.split("").map((c) => c + c).join("") : s;
  const n = parseInt(full, 16);
  if (!Number.isFinite(n) || full.length !== 6) return [0, 0, 0];
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
}

/* A role colour is a triple: deep (shadow side), base (the app token), hi (the
   lit core). The page passes the triples from voice.css (`--rt-*-deep`, `--rt-*`,
   `--rt-*-hi`); a plain colour gets its deep and hi shades mixed here. */
function tri(c, bg) {
  if (c && typeof c === "object") {
    return { deep: hexRgb(c.deep), base: hexRgb(c.base), hi: hexRgb(c.hi) };
  }
  const base = hexRgb(c);
  const b = hexRgb(bg);
  const deep = base.map((v, i) => b[i] + (v - b[i]) * 0.42);
  const hi = base.map((v) => v + (1 - v) * 0.7);
  return { deep, base, hi };
}

/* The sample's palette, the fallback for anything the page leaves out. */
const DEFAULTS = {
  bg: "#121518",
  accent: { deep: "#0F4F7A", base: "#3DAEE9", hi: "#C4E9FB" },
  accent2: { deep: "#163E5A", base: "#5AA9D6", hi: "#CFE6F4" },
  muted: { deep: "#1E2A35", base: "#4F6476", hi: "#9DB0C0" },
  warn: { deep: "#6E420C", base: "#E8A33D", hi: "#FFE0AE" },
};

export function palette(p) {
  const q = Object.assign({}, DEFAULTS, p || {});
  return {
    bg: hexRgb(typeof q.bg === "object" ? q.bg.base : q.bg),
    assistant: tri(q.accent, q.bg),
    thinking: tri(q.accent2, q.bg),
    idle: tri(q.muted, q.bg),
    user: tri(q.warn, q.bg),
  };
}

// ----------------------------------------------------------------- signal
/* Tunables for turning an AnalyserNode into a level and BANDS bands. */
export const SIGNAL = {
  fftSize: 2048, // 1024 bins, ~23 Hz each at 48 kHz
  analyserSmoothing: 0.0, // smoothing happens below, identically for every source
  fMin: 90,
  fMax: 7600, // band range in Hz, log-spaced (where speech lives)
  floorDb: -84, // band level that maps to 0
  rangeDb: 50, // dB above the floor that maps to 1
  tiltDbPerOct: 3.5, // lift the highs so a voice's falling spectrum fills the display
  levelFloorDb: -50, // RMS dBFS that maps to level 0 ...
  levelRangeDb: 38, // ... and to 1 at -12 dBFS
  levelAttack: 0.035,
  levelRelease: 0.2, // envelope follower time constants (s)
  bandAttack: 0.025,
  bandRelease: 0.16,
};

/* One-pole envelope follower: quick when rising (attack), slower when falling
   (release). k = 1 - e^(-dt/tau) keeps it independent of the frame rate. */
export function follow(cur, target, dt, attack, release) {
  const tau = target > cur ? attack : release;
  return cur + (target - cur) * (1 - Math.exp(-dt / tau));
}

const BAND_TILT = new Float32Array(BANDS);
for (let b = 0; b < BANDS; b++) {
  const f0 = SIGNAL.fMin * Math.pow(SIGNAL.fMax / SIGNAL.fMin, b / BANDS);
  const f1 = SIGNAL.fMin * Math.pow(SIGNAL.fMax / SIGNAL.fMin, (b + 1) / BANDS);
  BAND_TILT[b] = SIGNAL.tiltDbPerOct * (Math.log2(Math.sqrt(f0 * f1)) - Math.log2(500));
}

/* A Tap is one role's view of the audio: raw targets each frame (tLevel,
   tBands) and the smoothed values the renderers read (level, bands). The
   analyser's own fftSize and smoothing are set while attached and given back
   at detach: the analysers are the player's and the capture's. */
class Tap {
  constructor() {
    this.level = 0;
    this.bands = new Float32Array(BANDS);
    this.tLevel = 0;
    this.tBands = new Float32Array(BANDS);
    this.analyser = null;
    this.saved = null;
  }
  attach(analyser) {
    this.detach();
    if (!analyser) return;
    this.analyser = analyser;
    this.saved = { fft: analyser.fftSize, smooth: analyser.smoothingTimeConstant };
    try {
      analyser.fftSize = SIGNAL.fftSize;
      analyser.smoothingTimeConstant = SIGNAL.analyserSmoothing;
    } catch (_) {
      /* a closed context's node: read as it is */
    }
    const bins = analyser.frequencyBinCount;
    this.freq = new Float32Array(bins);
    this.time = new Float32Array(analyser.fftSize);
    this.lo = new Int32Array(BANDS);
    this.hi = new Int32Array(BANDS);
    const rate = (analyser.context && analyser.context.sampleRate) || 48000;
    const hz = rate / 2 / bins;
    for (let b = 0; b < BANDS; b++) {
      const f0 = SIGNAL.fMin * Math.pow(SIGNAL.fMax / SIGNAL.fMin, b / BANDS);
      const f1 = SIGNAL.fMin * Math.pow(SIGNAL.fMax / SIGNAL.fMin, (b + 1) / BANDS);
      const lo = Math.max(1, Math.min(bins - 1, Math.floor(f0 / hz)));
      this.lo[b] = lo;
      this.hi[b] = Math.max(lo + 1, Math.min(bins, Math.ceil(f1 / hz)));
    }
  }
  detach() {
    const a = this.analyser;
    if (a && this.saved) {
      try {
        a.fftSize = this.saved.fft;
        a.smoothingTimeConstant = this.saved.smooth;
      } catch (_) {
        /* closed: nothing to give back */
      }
    }
    this.analyser = null;
    this.saved = null;
  }
  /* Fill this frame's targets from the analyser: mean power per band in dB,
     tilt-compensated and normalised; level from the time-domain RMS. */
  read() {
    const a = this.analyser;
    if (!a) {
      this.zero();
      return;
    }
    a.getFloatFrequencyData(this.freq);
    for (let b = 0; b < BANDS; b++) {
      let p = 0;
      const lo = this.lo[b];
      const hi = this.hi[b];
      for (let i = lo; i < hi; i++) {
        const d = this.freq[i];
        if (d > -200) p += Math.pow(10, d * 0.1);
      }
      p /= hi - lo;
      const db = p > 0 ? 10 * Math.log10(p) : -200;
      this.tBands[b] = clamp01((db + BAND_TILT[b] - SIGNAL.floorDb) / SIGNAL.rangeDb);
    }
    a.getFloatTimeDomainData(this.time);
    let s = 0;
    for (let i = 0; i < this.time.length; i++) s += this.time[i] * this.time[i];
    const rms = Math.sqrt(s / this.time.length);
    this.tLevel = clamp01((20 * Math.log10(rms + 1e-9) - SIGNAL.levelFloorDb) / SIGNAL.levelRangeDb);
  }
  zero() {
    this.tLevel = 0;
    this.tBands.fill(0);
  }
  smooth(dt) {
    this.level = follow(this.level, this.tLevel, dt, SIGNAL.levelAttack, SIGNAL.levelRelease);
    for (let b = 0; b < BANDS; b++) {
      this.bands[b] = follow(this.bands[b], this.tBands[b], dt, SIGNAL.bandAttack, SIGNAL.bandRelease);
    }
  }
}

// ------------------------------------------------- state machine, mix weights
/* Five states. Renderers never switch on the state for their look: they read
   `mix`, five weights that always sum to 1, so any change between any two
   states (even mid-transition) is a smooth crossfade. The page decides when
   `interrupted` ends (its INTERRUPTED_MS); the machine only eases. */
export const STATE = {
  transition: 0.32, // s, eased crossfade between any two states
  cutIn: 0.12, // s, the barge-in cut is deliberately quicker
  reducedTransition: 0.5, // s, reduced motion: slower, softer crossfades
};

class StateMachine {
  constructor() {
    this.state = S.IDLE;
    this.prev = S.IDLE;
    this.t0 = -100;
    this.dur = STATE.transition;
    this.mixFrom = new Float32Array(5);
    this.mix = new Float32Array(5);
    this.mix[S.IDLE] = 1;
    this.mixFrom[S.IDLE] = 1;
    this.blend = 1;
    this.cutT0 = -100;
    this.reduced = false;
  }
  /* Snapshot the current weights and ease from them to the new state, so a
     change during a transition continues from wherever the mix is. */
  set(s, now) {
    if (s === this.state) return false;
    this.mixFrom.set(this.mix);
    this.prev = this.state;
    this.state = s;
    this.t0 = now;
    this.dur = this.reduced ? STATE.reducedTransition : s === S.INTERRUPTED ? STATE.cutIn : STATE.transition;
    if (s === S.INTERRUPTED) this.cutT0 = now;
    return true;
  }
  update(now) {
    const p = clamp01((now - this.t0) / this.dur);
    const e = p * p * (3 - 2 * p); // smoothstep: eased in and out
    for (let i = 0; i < 5; i++) this.mix[i] = this.mixFrom[i] + ((i === this.state ? 1 : 0) - this.mixFrom[i]) * e;
    this.blend = e;
  }
}

/* The barge-in "cut": a one-shot envelope, quick rise then decay, that the
   renderers use for their collapse. 0 outside a barge-in. */
function cutEnvelope(age) {
  if (age < 0 || age > 1.5) return 0;
  return smoothstep(0, 0.05, age) * Math.exp(-Math.max(0, age - 0.05) / 0.25);
}

/* Colour = state weights x per-state triples. Blue and the user colour sit far
   apart, and halfway between them RGB goes grey, so the intensity dips
   slightly mid-change: the swap reads as a dissolve, not a muddy flash. */
function mixTint(m, P, out) {
  const tris = [P.idle, P.user, P.thinking, P.assistant, P.user];
  const wa = m[S.THINKING] + m[S.SPEAKING];
  const wu = m[S.LISTENING] + m[S.INTERRUPTED];
  const dim = 1 - 0.28 * (4 * wa * wu);
  for (let c = 0; c < 3; c++) {
    let d = 0;
    let b = 0;
    let h = 0;
    for (let s = 0; s < 5; s++) {
      d += m[s] * tris[s].deep[c];
      b += m[s] * tris[s].base[c];
      h += m[s] * tris[s].hi[c];
    }
    out.deep[c] = d * dim;
    out.base[c] = b * dim;
    out.hi[c] = h * dim;
  }
}

// ------------------------------------------------------------ drawing helpers
export function css(rgb, a = 1, mul = 1) {
  return `rgba(${Math.round(clamp01(rgb[0] * mul) * 255)},${Math.round(clamp01(rgb[1] * mul) * 255)},${Math.round(
    clamp01(rgb[2] * mul) * 255,
  )},${a.toFixed(3)})`;
}
/* Average of bands [a, b) — renderers use a low/mid/high summary. */
export function bandMean(bands, a, b) {
  let s = 0;
  for (let i = a; i < b; i++) s += bands[i];
  return s / (b - a);
}
const _mix = [new Float32Array(3), new Float32Array(3), new Float32Array(3)];
let _mixI = 0;
/* Small rotating pool so callers can use a few mixes per frame without allocating. */
export function mixRgb(a, b, t) {
  const o = _mix[(_mixI = (_mixI + 1) % 3)];
  for (let c = 0; c < 3; c++) o[c] = a[c] + (b[c] - a[c]) * t;
  return o;
}

// ---------------------------------------------------------------- the mount
const nowS = () => performance.now() / 1000;

/* The calm rate: idle and quiet, or reduced motion. */
export const IDLE_FPS = 15;
/* How long idle and quiet lasts before the calm rate (the followers settle). */
export const QUIET_S = 1.5;
/* A tap below this level, its target zero, is quiet. */
const QUIET_LEVEL = 0.005;

/* Mount a renderer on `canvas`; see the module doc for the handle. */
export function run(canvas, inputs, factory) {
  inputs = inputs || {};
  const P = palette(inputs.palette);
  const outTap = new Tap();
  const micTap = new Tap();
  const sm = new StateMachine();
  const F = {
    t: 0,
    dt: 0, // engine clock (s) and frame delta
    state: S.IDLE,
    prevState: S.IDLE, // discrete state, for one-shot effects only
    stateAge: 0,
    stateBlend: 1, // s since the last change; eased 0..1 progress of it
    mix: sm.mix, // Float32Array(5) state weights, sum 1 (index = S.*)
    outLevel: 0,
    outBins: outTap.bands, // assistant level 0..1 and BANDS bands 0..1
    micLevel: 0,
    micBins: micTap.bands, // user level and bands
    drive: 0,
    bands: new Float32Array(BANDS), // what to react to now: the mix of the two roles
    cut: 0,
    cutAge: 99, // barge-in envelope 0..1 and seconds since it
    tint: { deep: new Float32Array(3), base: new Float32Array(3), hi: new Float32Array(3) },
    userTri: P.user, // the user's colour (for the cut flash)
    bg: P.bg,
    bgCss: css(P.bg),
    width: 0,
    height: 0,
    dpr: 1,
    reduced: !!inputs.reducedMotion,
  };
  sm.reduced = F.reduced;
  outTap.attach(inputs.output || null);
  micTap.attach(inputs.input || null);

  let cv = canvas;
  let r = factory(cv, { palette: P });
  let muted = false;
  let timing = null;
  let raf = 0;
  let timer = 0;
  let quietSince = -1;
  let calm = false;
  let last = 0;
  let destroyed = false;
  let visible = true;
  let frames = 0;
  let perf = 0;
  let size = [0, 0, 1];
  const replaced = [];

  const step = (t, dt) => {
    F.t = t;
    F.dt = dt;
    outTap.read();
    if (muted) micTap.zero();
    else micTap.read();
    outTap.smooth(dt);
    micTap.smooth(dt);
    sm.update(t);
    F.state = sm.state;
    F.prevState = sm.prev;
    F.stateAge = t - sm.t0;
    F.stateBlend = sm.blend;
    F.outLevel = outTap.level;
    F.micLevel = micTap.level;
    const m = sm.mix;
    F.drive = m[S.LISTENING] * micTap.level + m[S.SPEAKING] * outTap.level;
    for (let b = 0; b < BANDS; b++) F.bands[b] = m[S.LISTENING] * micTap.bands[b] + m[S.SPEAKING] * outTap.bands[b];
    F.cutAge = t - sm.cutT0;
    F.cut = cutEnvelope(F.cutAge);
    mixTint(m, P, F.tint);
  };

  /* A WebGL context lost mid-run: the factory's fallback takes a fresh canvas
     in its place (a canvas that had a webgl2 context gives no 2d one). */
  const checkLost = () => {
    if (!r.lost || !r.fallback) return;
    const c2 = document.createElement("canvas");
    c2.className = cv.className;
    c2.setAttribute("style", cv.getAttribute("style") || "");
    cv.replaceWith(c2);
    replaced.push(cv);
    cv = c2;
    const old = r;
    r = r.fallback(cv);
    if (old.release) old.release();
    r.resize(size[0], size[1], size[2]);
    // The handle says what draws now, not what drew until the swap (WP9
    // review NIT 10).
    handle.kind = r.kind;
  };

  /* A pixel ratio that changes with no size change (another monitor, the
     page's zoom) says nothing to the ResizeObserver: a media query on the
     ratio in effect re-sizes the backing store then (WP9 review NIT 10).
     Only while the host follows the window's ratio. */
  let dprWatch = null;
  const watchDpr = () => {
    if (dprWatch) dprWatch.mq.removeEventListener("change", dprWatch.on);
    dprWatch = null;
    const now = window.devicePixelRatio || 1;
    if (destroyed || typeof matchMedia !== "function" || size[2] !== now) return;
    const mq = matchMedia(`(resolution: ${now}dppx)`);
    const on = () => {
      if (!destroyed && size[0] > 0) handle.resize(size[0], size[1], window.devicePixelRatio || 1);
    };
    mq.addEventListener("change", on);
    dprWatch = { mq, on };
  };

  const frame = (ms) => {
    raf = 0;
    if (destroyed) return;
    const t = ms / 1000;
    // A calm frame comes 1/IDLE_FPS after the last: its delta is allowed
    // that long, so the followers keep their time.
    const dt = last ? Math.min(calm ? 0.1 : 0.05, Math.max(0, t - last)) : 1 / 60;
    last = t;
    step(t, dt);
    const quiet = sm.state === S.IDLE && sm.blend >= 1 && outTap.tLevel === 0 && micTap.tLevel === 0
      && outTap.level < QUIET_LEVEL && micTap.level < QUIET_LEVEL;
    if (!quiet) quietSince = -1;
    else if (quietSince < 0) quietSince = t;
    calm = F.reduced || (quiet && t - quietSince >= QUIET_S);
    checkLost();
    const t0 = performance.now();
    r.render(F);
    const took = performance.now() - t0;
    perf = perf ? perf * 0.95 + took * 0.05 : took;
    frames++;
    schedule();
  };
  const running = () => !destroyed && visible && !document.hidden && F.width > 0 && F.height > 0;
  const schedule = () => {
    if (raf || timer || !running()) return;
    if (calm) {
      timer = setTimeout(() => {
        timer = 0;
        if (!raf && running()) raf = requestAnimationFrame(frame);
      }, 1000 / IDLE_FPS);
    } else {
      raf = requestAnimationFrame(frame);
    }
  };
  /* Back to the full rate now (a state change, a resize). */
  const wake = () => {
    quietSince = -1;
    calm = F.reduced;
    if (timer && !calm) {
      clearTimeout(timer);
      timer = 0;
    }
    schedule();
  };
  const pause = () => {
    if (raf) cancelAnimationFrame(raf);
    if (timer) clearTimeout(timer);
    raf = 0;
    timer = 0;
    last = 0;
  };
  const onVisibility = () => (document.hidden ? pause() : schedule());
  document.addEventListener("visibilitychange", onVisibility);
  let io = null;
  if (typeof IntersectionObserver === "function") {
    io = new IntersectionObserver((entries) => {
      for (const e of entries) visible = e.isIntersecting;
      if (visible) schedule();
      else pause();
    });
    io.observe(canvas.parentElement || canvas);
  }

  const handle = {
    kind: r.kind,
    setState(state, info) {
      const s = STATE_NAMES.indexOf(state);
      if (s < 0) return;
      muted = !!(info && info.muted);
      if (sm.set(s, nowS())) wake();
      else schedule();
    },
    setTiming(t) {
      timing = t || null;
    },
    setInputs(next) {
      if (!next) return;
      if ("output" in next && next.output !== outTap.analyser) outTap.attach(next.output || null);
      if ("input" in next && next.input !== micTap.analyser) micTap.attach(next.input || null);
      wake();
    },
    resize(w, h, dpr) {
      size = [Math.max(0, Math.round(w)), Math.max(0, Math.round(h)), dpr || 1];
      F.width = size[0];
      F.height = size[1];
      F.dpr = size[2];
      r.resize(size[0], size[1], size[2]);
      handle.kind = r.kind;
      watchDpr();
      wake();
    },
    stats() {
      return { kind: r.kind, frames, ms: +perf.toFixed(3), running: !!(raf || timer), calm, destroyed,
        timing: !!timing };
    },
    destroy() {
      if (destroyed) return;
      destroyed = true;
      pause();
      document.removeEventListener("visibilitychange", onVisibility);
      if (io) io.disconnect();
      watchDpr();
      outTap.detach();
      micTap.detach();
      if (r.release) r.release();
      // The backing store goes with it, and a canvas the fallback replaced
      // is put back where the page made it.
      cv.width = 0;
      cv.height = 0;
      if (replaced.length) {
        cv.replaceWith(replaced[0]);
        replaced[0].width = 0;
        replaced[0].height = 0;
      }
    },
  };
  return handle;
}
