// The mirrored waveform ribbon (chat-voice §10, variant C of the owner's
// sample): layered lens shapes between mirrored curves, widest and flattest —
// it uses the panel's width, so it still works in a short panel. Plain
// Canvas2D. The default in the in-chat realtime panel.

import { run, BANDS, S, TAU, follow, css, bandMean, mixRgb } from "./engine.js";

/* Tunables (the sample's, unchanged). */
export const RIBBON = {
  span: 0.9, // share of the canvas width the ribbon covers
  amp: 0.44, // max half-height at full drive, fraction of the height
  restAmp: 0.01, // half-height when silent (a living hairline)
  step: 3, // CSS px between curve samples
  taper: 1.7, // envelope at the ends: sin(pi u)^taper
  // Layers, front to back. k: wave numbers across the span, w: phase speed (rad/s),
  // gain: amplitude share, alpha: fill strength, deep: how far the colour sinks
  // toward the deep shade, lag: extra release (s) so back layers trail the front.
  layers: [
    { k: [1.7, 2.9, 4.6], w: [1.3, -1.9, 2.6], gain: 1.0, alpha: 0.8, deep: 0.0, lag: 0.0 },
    { k: [1.3, 2.3, 3.6], w: [-1.0, 1.5, -2.1], gain: 0.86, alpha: 0.46, deep: 0.3, lag: 0.07 },
    { k: [1.0, 1.8, 2.8], w: [0.7, -1.1, 1.6], gain: 0.72, alpha: 0.3, deep: 0.5, lag: 0.14 },
    { k: [0.8, 1.4, 2.1], w: [-0.5, 0.8, -1.2], gain: 0.58, alpha: 0.2, deep: 0.65, lag: 0.22 },
  ],
  thinkPeriod: 1.6, // s per outward swell while thinking
  thinkAmp: 0.11, // swell half-height, fraction of the height
  cutSqueeze: 0.4, // the ribbon narrows toward the centre at a barge-in
};

function createRibbon(canvas) {
  const ctx = canvas.getContext("2d", { alpha: false });
  const P = RIBBON;
  const NL = P.layers.length;
  const ld = new Float32Array(NL);
  let n = 0;
  let A = null;
  let X = null;
  let low = 0;
  let mid = 0;
  let high = 0;
  return {
    kind: "Canvas2D",
    resize(w, h, dpr) {
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
      n = Math.max(32, Math.ceil((w * P.span) / P.step) + 1);
      A = new Float32Array(n);
      X = new Float32Array(n);
    },
    render(F) {
      if (!A) return;
      const W = F.width;
      const H = F.height;
      const cy = H / 2;
      const m = F.mix;
      const motion = F.reduced ? 0.35 : 1;
      const T = F.t * motion;
      const tb = F.tint.base;
      const th = F.tint.hi;
      const td = F.tint.deep;
      const cut = F.cut * (F.reduced ? 0.4 : 1);
      ctx.setTransform(F.dpr, 0, 0, F.dpr, 0, 0);
      ctx.globalCompositeOperation = "source-over";
      ctx.shadowBlur = 0;
      ctx.fillStyle = F.bgCss;
      ctx.fillRect(0, 0, W, H);
      low = follow(low, bandMean(F.bands, 0, 12), F.dt, 0.05, 0.2);
      mid = follow(mid, bandMean(F.bands, 12, 30), F.dt, 0.04, 0.16);
      high = follow(high, bandMean(F.bands, 30, BANDS), F.dt, 0.03, 0.12);
      const span = W * P.span * (1 - P.cutSqueeze * cut);
      const x0 = (W - span) / 2;
      for (let i = 0; i < n; i++) X[i] = x0 + (i / (n - 1)) * span;
      const breath = 1 + 0.5 * Math.sin(F.t * TAU * 0.16 * motion);
      const ph = ((F.t / P.thinkPeriod) * (F.reduced ? 0.6 : 1)) % 1;
      const think = m[S.THINKING];
      const a0 = 0.25 + low;
      const a1 = 0.18 + mid;
      const a2 = 0.1 + high * 0.9;
      const an = a0 + a1 + a2;

      // centre hairline, always there: the panel's resting line
      let g = ctx.createLinearGradient(x0, 0, x0 + span, 0);
      g.addColorStop(0, css(tb, 0));
      g.addColorStop(0.5, css(tb, 0.3));
      g.addColorStop(1, css(tb, 0));
      ctx.fillStyle = g;
      ctx.fillRect(x0, cy - 0.5, span, 1);

      ctx.globalCompositeOperation = "lighter";
      for (let L = NL - 1; L >= 0; L--) {
        const lay = P.layers[L];
        ld[L] = follow(ld[L], F.drive, F.dt, 0.03 + lay.lag * 0.5, 0.18 + lay.lag * 2.5);
        const active = H * P.amp * ld[L] * lay.gain * (1 - cut);
        const rest = H * P.restAmp * breath * (1 - 0.7 * cut) * (L === 0 ? 1 : 0.6);
        let maxA = 1;
        for (let i = 0; i < n; i++) {
          const u = i / (n - 1);
          const env = Math.pow(Math.sin(Math.PI * u), P.taper);
          const w =
            Math.abs(
              a0 * Math.sin(TAU * lay.k[0] * u + lay.w[0] * T + L * 1.7) +
                a1 * Math.sin(TAU * lay.k[1] * u + lay.w[1] * T + L * 0.9) +
                a2 * Math.sin(TAU * lay.k[2] * u + lay.w[2] * T + L * 2.3),
            ) / an;
          let a = env * (w * active + rest * (0.6 + 0.4 * Math.sin(TAU * 1.3 * u + T * 0.8 + L)));
          if (think > 0.001) {
            // two swells running outward from the centre
            const d1 = (u - (0.5 + 0.5 * ph)) / 0.07;
            const d2 = (u - (0.5 - 0.5 * ph)) / 0.07;
            const swell = (Math.exp(-d1 * d1) + Math.exp(-d2 * d2)) * Math.sin(Math.PI * ph);
            a +=
              think *
              H *
              (0.02 * env + P.thinkAmp * swell * env * (F.reduced ? 0.5 : 1)) *
              (L === 0 ? 1 : 0.7 - L * 0.12);
          }
          A[i] = a;
          if (a > maxA) maxA = a;
        }
        // filled lens shapes between the mirrored curves
        ctx.beginPath();
        ctx.moveTo(X[0], cy - A[0]);
        for (let i = 1; i < n; i++) ctx.lineTo(X[i], cy - A[i]);
        for (let i = n - 1; i >= 0; i--) ctx.lineTo(X[i], cy + A[i]);
        ctx.closePath();
        const base = mixRgb(tb, td, lay.deep);
        g = ctx.createLinearGradient(0, cy - maxA, 0, cy + maxA);
        g.addColorStop(0, css(base, lay.alpha * 0.38));
        g.addColorStop(0.5, css(L === 0 ? mixRgb(tb, th, 0.45) : base, lay.alpha * 0.85));
        g.addColorStop(1, css(base, lay.alpha * 0.38));
        ctx.fillStyle = g;
        ctx.fill();
        if (L === 0) {
          // crisp, glowing edge on the front layer
          ctx.beginPath();
          ctx.moveTo(X[0], cy - A[0]);
          for (let i = 1; i < n; i++) ctx.lineTo(X[i], cy - A[i]);
          ctx.moveTo(X[0], cy + A[0]);
          for (let i = 1; i < n; i++) ctx.lineTo(X[i], cy + A[i]);
          ctx.lineJoin = "round";
          ctx.lineWidth = 1.3;
          ctx.shadowColor = css(tb, 0.9);
          ctx.shadowBlur = 8 * F.dpr;
          ctx.strokeStyle = css(mixRgb(tb, th, 0.55), 0.95);
          ctx.stroke();
          ctx.shadowBlur = 0;
        }
      }
      if (cut > 0.01) {
        // the cut: a bright flash along the centre line in the user's colour
        g = ctx.createLinearGradient(x0, 0, x0 + span, 0);
        g.addColorStop(0, css(F.userTri.hi, 0));
        g.addColorStop(0.5, css(F.userTri.hi, 0.85 * cut));
        g.addColorStop(1, css(F.userTri.hi, 0));
        ctx.fillStyle = g;
        ctx.fillRect(x0, cy - 1, span, 2);
      }
      ctx.globalCompositeOperation = "source-over";
    },
  };
}

export function mount(canvas, inputs) {
  return run(canvas, inputs, createRibbon);
}
