// The glowing orb (chat-voice §10, variant A of the owner's sample): one soft
// body that breathes and swells with the voice — the most "presence", read at
// a glance from across the room. A WebGL2 fragment shader, with a Canvas2D
// fallback that draws the same parameters (no WebGL2, a shader that does not
// compile, or a context lost mid-run). The default in the larger focus view.

import { run, BANDS, S, TAU, follow, css, bandMean, mixRgb } from "./engine.js";

/* Tunables for both paths (the sample's, unchanged). */
export const ORB = {
  radius: 0.22, // base radius, fraction of min(width, height)
  breathAmp: 0.03, // idle breathing, radius fraction
  breathHz: 0.16,
  grow: 0.17, // extra radius at full drive
  thinkShrink: 0.07, // smaller while thinking ...
  thinkPulseHz: 0.85, // ... with a slow inward pulse
  cutShrink: 0.3, // radius lost at the moment of a barge-in
  dispBase: 0.01, // resting edge wobble, radius fraction
  dispLow: 0.05,
  dispMid: 0.03,
  dispHigh: 0.012, // edge displacement per band group at full energy
  glowNear: 0.36,
  glowFar: 0.7, // glow falloff lengths, radius multiples
  glowGain: 1.0,
  sheenSpeed: 0.85, // rad/s of the thinking sheen around the rim (one turn in ~7 s)
  ringLife: 0.55, // s, barge-in shockwave
  ringReach: 1.25, // radii travelled by the shockwave over its life
  bodyAttack: 0.06,
  bodyRelease: 0.24, // the body's size follows the drive more slowly than the edge
};

const ORB_VS = `#version 300 es
void main() { vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)); gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0); }`;
const ORB_FS = `#version 300 es
precision highp float;
uniform vec2 uRes;            // canvas size, device px
uniform float uTime;          // s
uniform float uR;             // orb radius, device px
uniform float uLevel;         // smoothed drive 0..1
uniform float uThink;         // thinking weight 0..1
uniform float uDpr;
uniform vec4 uDisp;           // edge displacement: low lobes, mid, ripples (device px); w = noise speed
uniform vec2 uGlow;           // near / far glow falloff, device px
uniform float uGlowGain;
uniform vec3 uBase, uHi, uDeep, uBg, uCutCol;
uniform vec3 uRing;           // barge-in shockwave: radius px, width px, alpha
uniform float uSheen;         // angle of the thinking sheen
out vec4 outColor;

// 3D simplex noise: Ian McEwan, Ashima Arts / Stefan Gustavson (MIT)
vec3 mod289(vec3 x) { return x - floor(x * (1.0 / 289.0)) * 289.0; }
vec4 mod289(vec4 x) { return x - floor(x * (1.0 / 289.0)) * 289.0; }
vec4 permute(vec4 x) { return mod289(((x * 34.0) + 1.0) * x); }
vec4 taylorInvSqrt(vec4 r) { return 1.79284291400159 - 0.85373472095314 * r; }
float snoise(vec3 v) {
  const vec2 C = vec2(1.0 / 6.0, 1.0 / 3.0);
  const vec4 D = vec4(0.0, 0.5, 1.0, 2.0);
  vec3 i = floor(v + dot(v, C.yyy));
  vec3 x0 = v - i + dot(i, C.xxx);
  vec3 g = step(x0.yzx, x0.xyz);
  vec3 l = 1.0 - g;
  vec3 i1 = min(g.xyz, l.zxy);
  vec3 i2 = max(g.xyz, l.zxy);
  vec3 x1 = x0 - i1 + C.xxx;
  vec3 x2 = x0 - i2 + C.yyy;
  vec3 x3 = x0 - D.yyy;
  i = mod289(i);
  vec4 p = permute(permute(permute(i.z + vec4(0.0, i1.z, i2.z, 1.0)) + i.y + vec4(0.0, i1.y, i2.y, 1.0)) + i.x + vec4(0.0, i1.x, i2.x, 1.0));
  float n_ = 0.142857142857;
  vec3 ns = n_ * D.wyz - D.xzx;
  vec4 j = p - 49.0 * floor(p * ns.z * ns.z);
  vec4 x_ = floor(j * ns.z);
  vec4 y_ = floor(j - 7.0 * x_);
  vec4 x = x_ * ns.x + ns.yyyy;
  vec4 y = y_ * ns.x + ns.yyyy;
  vec4 h = 1.0 - abs(x) - abs(y);
  vec4 b0 = vec4(x.xy, y.xy);
  vec4 b1 = vec4(x.zw, y.zw);
  vec4 s0 = floor(b0) * 2.0 + 1.0;
  vec4 s1 = floor(b1) * 2.0 + 1.0;
  vec4 sh = -step(h, vec4(0.0));
  vec4 a0 = b0.xzyw + s0.xzyw * sh.xxyy;
  vec4 a1 = b1.xzyw + s1.xzyw * sh.zzww;
  vec3 p0 = vec3(a0.xy, h.x);
  vec3 p1 = vec3(a0.zw, h.y);
  vec3 p2 = vec3(a1.xy, h.z);
  vec3 p3 = vec3(a1.zw, h.w);
  vec4 norm = taylorInvSqrt(vec4(dot(p0, p0), dot(p1, p1), dot(p2, p2), dot(p3, p3)));
  p0 *= norm.x; p1 *= norm.y; p2 *= norm.z; p3 *= norm.w;
  vec4 m = max(0.6 - vec4(dot(x0, x0), dot(x1, x1), dot(x2, x2), dot(x3, x3)), 0.0);
  m = m * m;
  return 42.0 * dot(m * m, vec4(dot(p0, x0), dot(p1, x1), dot(p2, x2), dot(p3, x3)));
}

void main() {
  vec2 p = gl_FragCoord.xy - 0.5 * uRes;
  float r = length(p);
  vec2 dir = r > 1e-3 ? p / r : vec2(1.0, 0.0);
  float T = uTime * uDisp.w;
  // Edge: noise sampled on the unit circle (wraps seamlessly), three scales.
  float e1 = snoise(vec3(dir * 0.9, T * 0.21));
  float e2 = snoise(vec3(dir * 1.9 + 5.2, T * 0.47));
  float e3 = snoise(vec3(dir * 3.8 - 2.7, T * 0.95));
  float R = uR + e1 * uDisp.x + e2 * uDisp.y + e3 * uDisp.z;
  float aa = 0.85 * uDpr;
  float inside = 1.0 - smoothstep(R - aa, R + aa, r);
  float q = clamp(r / max(R, 1.0), 0.0, 1.0);
  vec2 u = p / max(uR, 1.0);

  // Body, a lit glass sphere: deep colour, an inner glow from the centre that
  // swells with the voice, slow currents through it, a fresnel-like rim light
  // and a small highlight up-left.
  float flow = snoise(vec3(u * 0.8, T * 0.12 + 3.0));
  float flow2 = snoise(vec3(u * 1.5 + flow * 0.6, T * 0.24));
  float inner = exp(-q * q * 1.7) * (0.50 + 0.42 * uLevel) * (0.90 + 0.18 * flow2);
  vec3 body = uDeep * 0.95 + uBase * inner;
  float rim = pow(q, 3.6);
  body += mix(uBase, uHi, 0.45) * rim * (0.62 + 0.38 * uLevel);
  vec2 lp = u - vec2(-0.30, 0.34);
  body += uHi * exp(-dot(lp, lp) * 9.0) * (0.16 + 0.22 * uLevel);
  // (faded out toward the centre, where the angle is undefined)
  float sheen = pow(max(cos(atan(p.y, p.x) - uSheen), 0.0), 6.0) * uThink * smoothstep(0.35, 0.9, q);
  body += mix(uBase, uHi, 0.6) * sheen * rim * 0.55;

  // Glow outside the edge: a near halo that follows the voice and a faint far one.
  float d = max(r - R, 0.0);
  float glow = exp(-d / uGlow.x) * (0.34 + 0.62 * uLevel) + exp(-d / uGlow.y) * 0.10;
  glow *= uGlowGain * (1.0 + 0.5 * sheen);
  // fade the glow toward the top and bottom edges so it never ends in a hard line
  glow *= mix(0.2, 1.0, smoothstep(0.0, uRes.y * 0.2, min(gl_FragCoord.y, uRes.y - gl_FragCoord.y)));
  vec3 col = uBg + uBase * glow;
  col = mix(col, body, inside);

  // Barge-in shockwave in the user's colour.
  if (uRing.z > 0.0) { float x = (r - uRing.x) / uRing.y; col += uCutCol * exp(-x * x) * uRing.z; }
  // Dither: the long glow gradient bands visibly in 8 bits without it.
  float n = fract(sin(dot(gl_FragCoord.xy, vec2(12.9898, 78.233))) * 43758.5453);
  col += (n - 0.5) / 255.0;
  outColor = vec4(col, 1.0);
}`;

/* Per-frame orb parameters shared by the GL and 2D paths (CSS px). */
function orbParams(F, st) {
  const P = ORB;
  const m = F.mix;
  const motion = F.reduced ? 0.4 : 1;
  st.body = follow(st.body, F.drive, F.dt, P.bodyAttack, P.bodyRelease);
  st.low = follow(st.low, bandMean(F.bands, 0, 12), F.dt, 0.05, 0.2);
  st.mid = follow(st.mid, bandMean(F.bands, 12, 30), F.dt, 0.04, 0.16);
  st.high = follow(st.high, bandMean(F.bands, 30, BANDS), F.dt, 0.03, 0.12);
  const breath =
    Math.sin(F.t * TAU * P.breathHz * (F.reduced ? 0.7 : 1)) *
    P.breathAmp *
    (m[S.IDLE] + 0.5 * m[S.LISTENING] + 0.3 * m[S.SPEAKING]);
  const think = m[S.THINKING];
  const thinkR = think * (-P.thinkShrink + 0.022 * Math.sin(F.t * TAU * P.thinkPulseHz * motion));
  const cut = F.cut * (F.reduced ? 0.35 : 1);
  const minDim = Math.min(F.width, F.height);
  st.R0 = P.radius * minDim * (1 + breath + st.body * P.grow + thinkR);
  st.R = st.R0 * (1 - cut * P.cutShrink);
  const calm = (1 - 0.65 * think) * (1 - 0.8 * cut) * (F.reduced ? 0.5 : 1);
  st.dLow = st.R * (P.dispBase + P.dispLow * st.low) * calm;
  st.dMid = st.R * (P.dispBase * 0.6 + P.dispMid * st.mid) * calm;
  st.dHigh = st.R * (P.dispHigh * st.high) * calm;
  st.speed = motion * (1 + 0.9 * think);
  st.sheen = F.t * P.sheenSpeed * motion;
  st.think = think;
  st.ringA = 0;
  if (!F.reduced && F.cutAge < P.ringLife) {
    const a = F.cutAge / P.ringLife;
    st.ringR = st.R0 * (1 + a * P.ringReach);
    st.ringW = 1.5 + 9 * a;
    st.ringA = 0.55 * (1 - a) * (1 - a);
  }
  st.level = st.body;
}

function createOrbGL(canvas) {
  const gl = canvas.getContext("webgl2", {
    antialias: false,
    alpha: false,
    depth: false,
    stencil: false,
    premultipliedAlpha: false,
    preserveDrawingBuffer: false,
    powerPreference: "default",
  });
  if (!gl) return null;
  const shaders = [];
  const sh = (type, src) => {
    const s = gl.createShader(type);
    shaders.push(s);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error("orb shader: " + gl.getShaderInfoLog(s));
    return s;
  };
  const prog = gl.createProgram();
  const free = () => {
    for (const s of shaders) gl.deleteShader(s);
    gl.deleteProgram(prog);
  };
  try {
    gl.attachShader(prog, sh(gl.VERTEX_SHADER, ORB_VS));
    gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, ORB_FS));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error("orb link: " + gl.getProgramInfoLog(prog));
  } catch (e) {
    free();
    // The fallback draws on a fresh canvas: this one's context goes now,
    // not at the next garbage collection (WP9 review NIT 10).
    const ext = gl.getExtension("WEBGL_lose_context");
    if (ext) ext.loseContext();
    throw e;
  }
  const U = {};
  for (const n of ["uRes", "uTime", "uR", "uLevel", "uThink", "uDpr", "uDisp", "uGlow", "uGlowGain", "uBase", "uHi", "uDeep", "uBg", "uCutCol", "uRing", "uSheen"])
    U[n] = gl.getUniformLocation(prog, n);
  const vao = gl.createVertexArray();
  const st = { body: 0, low: 0, mid: 0, high: 0 };
  let lost = false;
  const onLost = (e) => {
    e.preventDefault();
    lost = true;
  };
  canvas.addEventListener("webglcontextlost", onLost);
  return {
    kind: "WebGL2",
    get lost() {
      return lost;
    },
    fallback: createOrb2D,
    resize(w, h, dpr) {
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
    },
    render(F) {
      if (lost) return;
      orbParams(F, st);
      const d = F.dpr;
      gl.viewport(0, 0, canvas.width, canvas.height);
      gl.useProgram(prog);
      gl.bindVertexArray(vao);
      gl.uniform2f(U.uRes, canvas.width, canvas.height);
      gl.uniform1f(U.uTime, F.t);
      gl.uniform1f(U.uR, st.R * d);
      gl.uniform1f(U.uLevel, st.level);
      gl.uniform1f(U.uThink, st.think);
      gl.uniform1f(U.uDpr, d);
      gl.uniform4f(U.uDisp, st.dLow * d, st.dMid * d, st.dHigh * d, st.speed);
      gl.uniform2f(U.uGlow, st.R0 * ORB.glowNear * d, st.R0 * ORB.glowFar * d);
      gl.uniform1f(U.uGlowGain, ORB.glowGain);
      gl.uniform3fv(U.uBase, F.tint.base);
      gl.uniform3fv(U.uHi, F.tint.hi);
      gl.uniform3fv(U.uDeep, F.tint.deep);
      gl.uniform3fv(U.uBg, F.bg);
      gl.uniform3fv(U.uCutCol, F.userTri.base);
      gl.uniform3f(U.uRing, st.ringA > 0 ? st.ringR * d : 0, (st.ringW || 1) * d, st.ringA);
      gl.uniform1f(U.uSheen, st.sheen);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    },
    /* Everything the GPU holds goes, the context with it. */
    release() {
      canvas.removeEventListener("webglcontextlost", onLost);
      if (!lost) {
        gl.deleteVertexArray(vao);
        free();
        const ext = gl.getExtension("WEBGL_lose_context");
        if (ext) ext.loseContext();
      }
    },
  };
}

/* Canvas2D fallback: the same parameters, drawn with gradients and a
   displaced polygon. Cheaper noise (summed sines) for the edge. */
function createOrb2D(canvas) {
  const ctx = canvas.getContext("2d", { alpha: false });
  const N = 120;
  const ex = new Float32Array(N);
  const ey = new Float32Array(N);
  const st = { body: 0, low: 0, mid: 0, high: 0 };
  return {
    kind: "Canvas2D",
    resize(w, h, dpr) {
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
    },
    render(F) {
      orbParams(F, st);
      const W = F.width;
      const H = F.height;
      const cx = W / 2;
      const cy = H / 2;
      const T = F.t * st.speed;
      const tb = F.tint.base;
      const th = F.tint.hi;
      const td = F.tint.deep;
      ctx.setTransform(F.dpr, 0, 0, F.dpr, 0, 0);
      ctx.globalCompositeOperation = "source-over";
      ctx.shadowBlur = 0;
      ctx.fillStyle = F.bgCss;
      ctx.fillRect(0, 0, W, H);
      const R = st.R;
      const R0 = st.R0;
      // glow
      ctx.globalCompositeOperation = "lighter";
      const ga = ORB.glowGain * (0.34 + 0.62 * st.level);
      // glow: an exponential falloff approximated by gradient stops, kept inside the canvas
      const gOut = Math.min(R0 * (1 + 4 * ORB.glowNear), H / 2 - 1);
      let g = ctx.createRadialGradient(cx, cy, R * 0.9, cx, cy, Math.max(gOut, R + 1));
      for (let i = 0; i <= 8; i++) {
        const f = i / 8;
        g.addColorStop(f, css(tb, ga * Math.exp(-4.2 * f) * (1 - f)));
      }
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, W, H);
      const gFar = Math.min(R0 * (1 + 3 * ORB.glowFar), H / 2 - 1);
      g = ctx.createRadialGradient(cx, cy, R, cx, cy, Math.max(gFar, R + 1));
      for (let i = 0; i <= 6; i++) {
        const f = i / 6;
        g.addColorStop(f, css(tb, 0.1 * ORB.glowGain * Math.exp(-3 * f) * (1 - f)));
      }
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, W, H);
      // edge polygon
      for (let i = 0; i < N; i++) {
        const a = (i / N) * TAU;
        const n1 = Math.sin(a * 2 + T * 0.9) * 0.6 + Math.sin(a * 3 - T * 0.7 + 1.3) * 0.4;
        const n2 = Math.sin(a * 5 + T * 1.7 + 0.4) * 0.6 + Math.sin(a * 7 - T * 1.3) * 0.4;
        const n3 = Math.sin(a * 11 + T * 2.9) * 0.5 + Math.sin(a * 13 - T * 2.3 + 2.0) * 0.5;
        const r = R + n1 * st.dLow + n2 * st.dMid + n3 * st.dHigh;
        ex[i] = cx + Math.cos(a) * r;
        ey[i] = cy + Math.sin(a) * r;
      }
      ctx.beginPath();
      ctx.moveTo((ex[N - 1] + ex[0]) / 2, (ey[N - 1] + ey[0]) / 2);
      for (let i = 0; i < N; i++) {
        const j = (i + 1) % N;
        ctx.quadraticCurveTo(ex[i], ey[i], (ex[i] + ex[j]) / 2, (ey[i] + ey[j]) / 2);
      }
      ctx.closePath();
      ctx.globalCompositeOperation = "source-over";
      ctx.fillStyle = css(td, 1, 0.95);
      ctx.fill();
      // inner glow, rim light and highlight, added on top of the deep body
      ctx.globalCompositeOperation = "lighter";
      const inner = 0.5 + 0.42 * st.level;
      g = ctx.createRadialGradient(cx, cy, 0, cx, cy, R);
      g.addColorStop(0, css(tb, inner));
      g.addColorStop(0.55, css(tb, inner * 0.55));
      g.addColorStop(1, css(tb, inner * 0.18));
      ctx.fillStyle = g;
      ctx.fill();
      g = ctx.createRadialGradient(cx, cy, R * 0.55, cx, cy, R * 1.02);
      g.addColorStop(0, css(th, 0));
      g.addColorStop(1, css(mixRgb(tb, th, 0.45), 0.62 + 0.38 * st.level));
      ctx.fillStyle = g;
      ctx.fill();
      g = ctx.createRadialGradient(cx - 0.3 * R, cy - 0.34 * R, 0, cx - 0.3 * R, cy - 0.34 * R, R * 0.4);
      g.addColorStop(0, css(th, 0.16 + 0.22 * st.level));
      g.addColorStop(1, css(th, 0));
      ctx.fillStyle = g;
      ctx.fill();
      if (st.think > 0.01) {
        ctx.lineCap = "round";
        ctx.lineWidth = Math.max(1.5, R * 0.06);
        ctx.strokeStyle = css(th, 0.5 * st.think);
        ctx.beginPath();
        ctx.arc(cx, cy, R * 0.93, -st.sheen - 0.5, -st.sheen + 0.5);
        ctx.stroke();
      }
      if (st.ringA > 0) {
        ctx.lineWidth = st.ringW;
        ctx.strokeStyle = css(F.userTri.base, st.ringA);
        ctx.beginPath();
        ctx.arc(cx, cy, st.ringR, 0, TAU);
        ctx.stroke();
      }
      ctx.globalCompositeOperation = "source-over";
    },
  };
}

/* WebGL2 where it works; a shader that fails after the context was made
   leaves that canvas to WebGL, so its fallback takes a fresh one (the engine
   swaps it in at the first frame, as for a context lost mid-run). */
function createOrb(canvas, opts) {
  if (!(opts && opts.forceCanvas2d)) {
    try {
      const r = createOrbGL(canvas);
      if (r) return r;
    } catch (e) {
      console.warn("lmgw voice orb: WebGL2 failed, drawing with Canvas2D:", e && e.message ? e.message : e);
      return { kind: "Canvas2D", lost: true, fallback: createOrb2D, resize() {}, render() {} };
    }
  }
  return createOrb2D(canvas);
}

export function mount(canvas, inputs) {
  const force = !!(inputs && inputs.forceCanvas2d);
  return run(canvas, inputs, (c, o) => createOrb(c, Object.assign({}, o, { forceCanvas2d: force })));
}
