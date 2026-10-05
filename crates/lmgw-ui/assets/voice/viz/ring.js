// The radial spectrum ring (chat-voice §10, variant B of the owner's sample):
// frequency bands around a circle, mirrored left/right — compact and
// technical, an instrument's look. Plain Canvas2D.

import { run, BANDS, S, TAU, follow, css, mixRgb } from "./engine.js";

/* Tunables (the sample's, unchanged). */
export const RING = {
  radius: 0.25, // ring radius, fraction of min(width, height)
  barSpacing: 7.5, // CSS px between bar centres on the ring: the bar count follows the
  minBars: 20,
  maxBars: 48, // circumference, per side (mirrored; low bands at the top)
  barFill: 0.46, // bar thickness as a share of the spacing
  minLen: 0.04, // resting bar length, fraction of the radius
  maxLen: 0.64, // bar length at full band energy
  inward: 0.3, // share of each bar drawn inside the ring line
  barAttack: 0.035,
  barRelease: 0.11, // per-bar smoothing on top of the band followers (s)
  glowPx: 9, // bar glow, CSS px
  idleWave: 0.06, // idle: a slow breathing swell, fraction of the radius
  thinkWave: 0.26, // thinking: crests running down both sides
  thinkSpeed: 0.55, // crests per second
  cutShrink: 0.1, // ring radius lost at a barge-in
  driveGrow: 0.06, // the ring itself grows a little with the voice
};

function createRing(canvas) {
  const ctx = canvas.getContext("2d", { alpha: false });
  const P = RING;
  const len = new Float32Array(P.maxBars);
  const sinA = new Float32Array(P.maxBars);
  const cosA = new Float32Array(P.maxBars);
  const tipX = new Float32Array(P.maxBars * 2);
  const tipY = new Float32Array(P.maxBars * 2);
  let half = P.minBars;
  let bars = half * 2;
  let drive = 0;
  return {
    kind: "Canvas2D",
    resize(w, h, dpr) {
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
      const R = P.radius * Math.min(w, h);
      half = Math.max(P.minBars, Math.min(P.maxBars, Math.round((Math.PI * R) / P.barSpacing)));
      bars = half * 2;
      for (let j = 0; j < half; j++) {
        const a = ((j + 0.5) / half) * Math.PI;
        sinA[j] = Math.sin(a);
        cosA[j] = Math.cos(a);
      }
    },
    render(F) {
      const W = F.width;
      const H = F.height;
      const cx = W / 2;
      const cy = H / 2;
      const m = F.mix;
      const motion = F.reduced ? 0.4 : 1;
      const tb = F.tint.base;
      const th = F.tint.hi;
      const td = F.tint.deep;
      drive = follow(drive, F.drive, F.dt, 0.05, 0.25);
      const cut = F.cut * (F.reduced ? 0.4 : 1);
      const R =
        P.radius *
        Math.min(W, H) *
        (1 + drive * P.driveGrow - cut * P.cutShrink + 0.012 * Math.sin(F.t * TAU * 0.16) * m[S.IDLE]);
      ctx.setTransform(F.dpr, 0, 0, F.dpr, 0, 0);
      ctx.globalCompositeOperation = "source-over";
      ctx.shadowBlur = 0;
      ctx.fillStyle = F.bgCss;
      ctx.fillRect(0, 0, W, H);

      // bar lengths: band energy, idle swell, thinking crests, barge-in retract
      const breath = 0.5 + 0.5 * Math.sin(F.t * TAU * 0.16 * motion);
      for (let j = 0; j < half; j++) {
        const fb = (j / (half - 1)) * (BANDS - 1);
        const b0 = Math.floor(fb);
        const b1 = Math.min(BANDS - 1, b0 + 1);
        const v = F.bands[b0] + (F.bands[b1] - F.bands[b0]) * (fb - b0);
        const u = j / (half - 1); // 0 top .. 1 bottom
        const idle = P.idleWave * breath * (0.6 + 0.4 * Math.sin(u * 5.0 + F.t * 0.9 * motion)) * m[S.IDLE];
        const crest = Math.pow(0.5 + 0.5 * Math.cos(TAU * (u * 1.25 - F.t * P.thinkSpeed * motion)), 4);
        const think = (0.05 + P.thinkWave * crest * (F.reduced ? 0.5 : 1)) * m[S.THINKING];
        const target = (P.minLen + idle + think + v * P.maxLen) * (1 - 0.9 * cut);
        len[j] = follow(len[j], target, F.dt, P.barAttack, P.barRelease);
      }
      // inner halo: light hugging the inside of the ring, the centre stays dark
      let g = ctx.createRadialGradient(cx, cy, R * 0.45, cx, cy, R);
      g.addColorStop(0, css(tb, 0));
      g.addColorStop(0.8, css(tb, 0.05 + 0.07 * drive));
      g.addColorStop(1, css(tb, 0.1 + 0.1 * drive));
      ctx.globalCompositeOperation = "lighter";
      ctx.fillStyle = g;
      ctx.beginPath();
      ctx.arc(cx, cy, R, 0, TAU);
      ctx.fill();
      // aura: a soft fill through the bar tips
      for (let j = 0; j < half; j++) {
        const r1 = R + (1 - P.inward) * len[j] * R;
        tipX[j] = cx + sinA[j] * r1;
        tipY[j] = cy - cosA[j] * r1;
        tipX[bars - 1 - j] = cx - sinA[j] * r1;
        tipY[bars - 1 - j] = cy - cosA[j] * r1;
      }
      ctx.globalCompositeOperation = "lighter";
      ctx.beginPath();
      ctx.moveTo((tipX[bars - 1] + tipX[0]) / 2, (tipY[bars - 1] + tipY[0]) / 2);
      for (let i = 0; i < bars; i++) {
        const k = (i + 1) % bars;
        ctx.quadraticCurveTo(tipX[i], tipY[i], (tipX[i] + tipX[k]) / 2, (tipY[i] + tipY[k]) / 2);
      }
      ctx.closePath();
      g = ctx.createRadialGradient(cx, cy, R * 0.9, cx, cy, R * (1 + P.maxLen));
      g.addColorStop(0, css(tb, 0.16 + 0.12 * drive));
      g.addColorStop(1, css(tb, 0));
      ctx.fillStyle = g;
      ctx.fill();
      // ring line (flashes in the user's colour at a barge-in)
      ctx.lineWidth = 1;
      ctx.strokeStyle = css(tb, 0.2 + 0.15 * drive);
      ctx.beginPath();
      ctx.arc(cx, cy, R, 0, TAU);
      ctx.stroke();
      if (cut > 0.01) {
        ctx.lineWidth = 1.5 + 2 * cut;
        ctx.strokeStyle = css(F.userTri.hi, 0.75 * cut);
        ctx.beginPath();
        ctx.arc(cx, cy, R, 0, TAU);
        ctx.stroke();
      }
      // bars: one path, stroked twice (glow pass, then a hot thin core)
      const spacing = (Math.PI * R) / half;
      const bw = Math.max(1.4, spacing * P.barFill);
      ctx.beginPath();
      for (let j = 0; j < half; j++) {
        const L = len[j] * R;
        const r0 = R - P.inward * L;
        const r1 = R + (1 - P.inward) * L;
        const s = sinA[j];
        const c = cosA[j];
        ctx.moveTo(cx + s * r0, cy - c * r0);
        ctx.lineTo(cx + s * r1, cy - c * r1);
        ctx.moveTo(cx - s * r0, cy - c * r0);
        ctx.lineTo(cx - s * r1, cy - c * r1);
      }
      ctx.lineCap = "round";
      g = ctx.createRadialGradient(cx, cy, R * 0.7, cx, cy, R * (1 + P.maxLen * 0.8));
      g.addColorStop(0, css(mixRgb(td, tb, 0.6)));
      g.addColorStop(0.45, css(tb));
      g.addColorStop(1, css(mixRgb(tb, th, 0.7)));
      ctx.globalCompositeOperation = "source-over";
      ctx.shadowColor = css(tb, 0.55 + 0.35 * drive);
      ctx.shadowBlur = P.glowPx * F.dpr;
      ctx.lineWidth = bw;
      ctx.strokeStyle = g;
      ctx.stroke();
      ctx.shadowBlur = 0;
      ctx.globalCompositeOperation = "lighter";
      ctx.lineWidth = Math.max(0.8, bw * 0.4);
      ctx.strokeStyle = css(th, 0.18 + 0.4 * drive);
      ctx.stroke();
      ctx.globalCompositeOperation = "source-over";
    },
  };
}

export function mount(canvas, inputs) {
  return run(canvas, inputs, createRing);
}
