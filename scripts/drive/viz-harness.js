// The realtime panel's visualisation variants alone (chat-voice §10, WP9),
// for scripts/drive/chat-voice-viz.json. Loaded into a dashboard page with a
// {"script": …} step; defines window.vizHarness:
//
//   start()            an AudioContext with two synthetic voices — formant
//                      filtered sawtooths, gated by a syllable LFO — each into
//                      an AnalyserNode (the "output" and "input" taps), behind
//                      a zero gain: nothing is audible (the drive also runs
//                      Chrome muted). Answers the context's state.
//   mount(variant, o)  a 640×184 box (the panel's size) at the page's top
//                      left, a canvas in it, the module imported from
//                      /voice/viz/<variant>.js and mounted with the taps and
//                      the panel's palette; answers {kind}. o.quiet: no taps
//                      (a quiet room, nothing playing); o.reduced: reduced
//                      motion; o.transparent: the transparent flag, and the
//                      box behind the canvas without a background of its own.
//   corners()          the alpha of the canvas's four corner pixels (a
//                      Canvas2D variant), far from what any variant draws.
//   rate(ms)           frames drawn over `ms`, and whether the engine draws
//                      at its calm rate (idle and quiet, or reduced motion).
//   state(name)        setState(name, {since, muted: false, loading: null,
//                      held: false}).
//   stats()            the handle's stats plus the canvas's size.
//   destroy()          destroy(), then checks after 400 ms that no frame was
//                      drawn since, the canvas's backing store is gone, the
//                      orb's WebGL2 context is lost, and the analysers have
//                      their fftSize and smoothing back. Answers {ok, …}.
//   unmount()          removes the box (after destroy).
(() => {
  const H = {};
  let ctx = null;
  let taps = null;
  let handle = null;
  let box = null;
  let canvas = null;
  const PALETTE = {
    bg: "#121518",
    fg: "#E2E6EA",
    accent: { deep: "#0F4F7A", base: "#3DAEE9", hi: "#C4E9FB" },
    accent2: { deep: "#163E5A", base: "#5AA9D6", hi: "#CFE6F4" },
    warn: { deep: "#6E420C", base: "#E8A33D", hi: "#FFE0AE" },
    muted: { deep: "#1E2A35", base: "#4F6476", hi: "#9DB0C0" },
  };

  function voice(ctx, f0, f1, f2, rate) {
    const src = ctx.createOscillator();
    src.type = "sawtooth";
    src.frequency.value = f0;
    const a = ctx.createBiquadFilter();
    a.type = "bandpass";
    a.frequency.value = f1;
    a.Q.value = 4;
    const b = ctx.createBiquadFilter();
    b.type = "bandpass";
    b.frequency.value = f2;
    b.Q.value = 6;
    const mix = ctx.createGain();
    mix.gain.value = 0;
    src.connect(a);
    src.connect(b);
    a.connect(mix);
    b.connect(mix);
    // Syllables: the gain swings with a slow LFO.
    const lfo = ctx.createOscillator();
    lfo.frequency.value = rate;
    const depth = ctx.createGain();
    depth.gain.value = 0.5;
    lfo.connect(depth);
    depth.connect(mix.gain);
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 1024;
    analyser.smoothingTimeConstant = 0.6;
    mix.connect(analyser);
    const zero = ctx.createGain();
    zero.gain.value = 0;
    analyser.connect(zero);
    zero.connect(ctx.destination);
    src.start();
    lfo.start();
    return analyser;
  }

  H.start = async () => {
    if (!ctx) {
      ctx = new AudioContext({ sampleRate: 24000 });
      taps = {
        output: voice(ctx, 180, 640, 1800, 4.4),
        input: voice(ctx, 130, 520, 1450, 3.9),
      };
    }
    try {
      await ctx.resume();
    } catch (_) {
      /* the state says it */
    }
    return ctx.state;
  };

  H.mount = async (variant, o = {}) => {
    box = document.createElement("div");
    box.id = "viz-harness";
    box.style.cssText =
      "position:fixed;left:0;top:0;width:640px;height:184px;z-index:99999;" +
      (o.transparent ? "" : "background:#121518;");
    canvas = document.createElement("canvas");
    canvas.style.cssText = "position:absolute;inset:0;width:100%;height:100%;display:block;";
    box.appendChild(canvas);
    document.body.appendChild(box);
    const mod = await import(`/voice/viz/${variant}.js`);
    handle = mod.mount(canvas, {
      output: o.quiet ? null : taps.output,
      input: o.quiet ? null : taps.input,
      palette: PALETTE,
      reducedMotion: !!o.reduced,
      transparent: !!o.transparent,
    });
    handle.resize(640, 184, window.devicePixelRatio || 1);
    return { kind: handle.kind, fft: taps.output.fftSize, smoothing: taps.output.smoothingTimeConstant };
  };

  H.state = (name) => {
    handle.setState(name, { since: performance.now(), muted: false, loading: null, held: false });
    return name;
  };

  H.rate = async (ms) => {
    const f0 = handle.stats().frames;
    await new Promise((r) => setTimeout(r, ms));
    const s = handle.stats();
    return { frames: s.frames - f0, calm: s.calm };
  };

  H.stats = () => Object.assign({ width: canvas.width, height: canvas.height }, handle.stats());

  H.corners = () => {
    const c = box.querySelector("canvas").getContext("2d");
    const w = canvas.width;
    const h = canvas.height;
    return [
      [0, 0],
      [w - 1, 0],
      [0, h - 1],
      [w - 1, h - 1],
    ].map(([x, y]) => c.getImageData(x, y, 1, 1).data[3]);
  };

  H.destroy = async () => {
    const live = handle.stats();
    const current = box.querySelector("canvas");
    let gl = null;
    if (live.kind === "WebGL2") gl = current.getContext("webgl2");
    handle.destroy();
    const f0 = handle.stats().frames;
    await new Promise((r) => setTimeout(r, 400));
    const after = handle.stats();
    const out = {
      kind: live.kind,
      frames_before: live.frames,
      frames_after_destroy: after.frames - f0,
      running: after.running,
      canvas: [current.width, current.height],
      canvases_in_box: box.querySelectorAll("canvas").length,
      gl_lost: gl ? gl.isContextLost() : null,
      fft_back: taps.output.fftSize === 1024 && taps.input.fftSize === 1024,
      smoothing_back: Math.abs(taps.output.smoothingTimeConstant - 0.6) < 1e-6,
      avg_ms: live.ms,
    };
    out.ok =
      out.frames_before > 10 &&
      out.frames_after_destroy === 0 &&
      !out.running &&
      out.canvas[0] === 0 &&
      out.canvas[1] === 0 &&
      out.canvases_in_box === 1 &&
      (out.gl_lost === null || out.gl_lost === true) &&
      out.fft_back &&
      out.smoothing_back;
    H.lastDestroy = out;
    return out;
  };

  H.unmount = () => {
    if (box) box.remove();
    box = null;
    canvas = null;
    handle = null;
    return true;
  };

  window.vizHarness = H;
  return "vizHarness ready";
})();
