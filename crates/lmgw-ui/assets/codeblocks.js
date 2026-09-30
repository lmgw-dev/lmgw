// Code-block decoration for chat messages: syntax highlighting plus the hover
// toolbar (language chip · Copy · Preview). Ported from the old Lit chat app
// (assets/chat/chat-app.js decorateCode/addCodeTools).
//
// This is JS rather than Rust because highlight.js is the highlighter: the
// wasm-side alternative (syntect + its grammar set) would add megabytes to the
// bundle for something a vendored 120 KB script already does. lmgw-ui keeps its
// own copy of the vendored file (assets/vendor/highlight.js) so nothing here
// depends on the old UI's /assets tree, which goes away at the P8 cutover.
//
// Math (KaTeX) and diagrams (Mermaid) are decorated here too, from the same
// entry point. Both libraries are vendored under assets/vendor/ and loaded
// lazily on first need, so a page without math or diagrams never fetches them.
//
// Contract with the Rust side (pages/chat.rs):
//   window.lmgwDecorateCode(mdEl, settled)  — called from the effect that
//     writes a message's rendered markdown, with `settled` false while the
//     message is still streaming.
//   window "lmgw-preview" CustomEvent {detail:{code, lang}} — fired by a
//     Preview button; the Chat component turns it into the preview modal.
import hljs from "./vendor/highlight.js";

// Languages whose fenced blocks get a "Preview" button — rendered live in a
// sandboxed iframe. `html`/`svg` are highlight.js aliases of `xml`.
const PREVIEWABLE = new Set(["html", "svg", "xhtml"]);

// Inline feather-style icons (currentColor; emoji render tiny on WebKitGTK).
const ICON_COPY =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>';
const ICON_CHECK =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6L9 17l-5-5"></path></svg>';
const ICON_EYE =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7-11-7-11-7z"></path><circle cx="12" cy="12" r="3"></circle></svg>';

function addCodeTools(code, lang) {
  const pre = code.parentElement;
  if (!pre) return;
  // Wrap <pre> so the toolbar pins to the block, not the scrollable inner
  // content (an absolutely-positioned child of an overflow:auto <pre> would
  // scroll away horizontally). innerHTML is rebuilt each frame, so re-wrap.
  let wrap = pre.parentElement;
  if (!wrap || !wrap.classList.contains("code-wrap")) {
    wrap = document.createElement("div");
    wrap.className = "code-wrap";
    pre.replaceWith(wrap);
    wrap.appendChild(pre);
  } else if (wrap.querySelector(":scope > .code-tools")) {
    return;
  }
  const raw = code.textContent || "";
  const bar = document.createElement("div");
  bar.className = "code-tools";
  if (lang) {
    const tag = document.createElement("span");
    tag.className = "lang";
    tag.textContent = lang;
    bar.appendChild(tag);
  }
  if (PREVIEWABLE.has(lang)) {
    const prev = document.createElement("button");
    prev.title = "Preview in a sandboxed frame";
    prev.innerHTML = `${ICON_EYE}<span>Preview</span>`;
    prev.addEventListener("click", () => {
      window.dispatchEvent(
        new CustomEvent("lmgw-preview", { detail: { code: raw, lang } })
      );
    });
    bar.appendChild(prev);
  }
  const copy = document.createElement("button");
  copy.title = "Copy code";
  copy.innerHTML = `${ICON_COPY}<span>Copy</span>`;
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(raw);
      copy.classList.add("ok");
      copy.innerHTML = `${ICON_CHECK}<span>Copied</span>`;
      setTimeout(() => {
        copy.classList.remove("ok");
        copy.innerHTML = `${ICON_COPY}<span>Copy</span>`;
      }, 1200);
    } catch (_) {}
  });
  bar.appendChild(copy);
  wrap.appendChild(bar);
}

// Highlight every code block under `root` and, once the message has settled,
// attach its toolbar. The markdown is re-parsed into fresh innerHTML on every
// update, so blocks are plain text again here — re-highlighting is clean (no
// double-wrapping).
window.lmgwDecorateCode = function (root, settled) {
  if (!root) return;
  for (const code of root.querySelectorAll("pre > code")) {
    const cls = [...code.classList].find((c) => c.startsWith("language-"));
    const lang = cls ? cls.slice(9).toLowerCase() : "";
    // Labeled+known → highlight live (cheap, single grammar). Unlabeled →
    // auto-detect, but only once settled (it tries every grammar, too costly
    // per frame). Labeled-unknown → leave plain (avoids a console warning).
    if (lang) {
      if (hljs.getLanguage(lang)) {
        try {
          hljs.highlightElement(code);
        } catch (_) {}
      }
    } else if (settled) {
      try {
        hljs.highlightElement(code);
      } catch (_) {}
    }
    // Toolbars only on settled code — buttons on a half-written block are
    // pointless, and innerHTML is rebuilt each frame so they'd churn anyway.
    if (settled) addCodeTools(code, lang);
  }
  decorateLinks(root);
  renderMath(root);
  if (settled) renderDiagrams(root);
};

// Reply links leave the dashboard: http(s)/mailto get a new browsing context
// (the desktop shell hands those to xdg-open) instead of navigating this
// window away. Relative and #fragment links stay in-app.
function decorateLinks(root) {
  for (const a of root.querySelectorAll("a[href]")) {
    // Same rule as safe_href (chat_markdown.rs): after dropping control
    // characters, a leading pair of slashes (or backslashes, which URL parsing
    // reads as slashes) is a scheme-relative link to another origin. It is
    // neutralised to "#" there; if one ever gets here, do the same.
    const h = (a.getAttribute("href") || "")
      .replace(/[\t\n\r]/g, "")
      .replace(/^[\x00-\x20]+|[\x00-\x20]+$/g, "");
    if (/^[\/\\]{2}/.test(h)) {
      a.setAttribute("href", "#");
    } else if (/^(https?:|mailto:)/i.test(h)) {
      a.target = "_blank";
      a.rel = "noopener noreferrer";
    }
  }
}

// ---- lazy library loading ------------------------------------------------

const loading = new Map();
function loadScript(src) {
  if (!loading.has(src)) {
    loading.set(
      src,
      new Promise((resolve, reject) => {
        const s = document.createElement("script");
        s.src = src;
        s.onload = resolve;
        s.onerror = () => {
          loading.delete(src);
          reject(new Error("could not load " + src));
        };
        document.head.appendChild(s);
      })
    );
  }
  return loading.get(src);
}

// Vendor URLs are relative to this module so a non-root public_url still works.
const vendorUrl = (p) => new URL("./vendor/" + p, import.meta.url).href;

// ---- math ----------------------------------------------------------------

let katexCss = false;
function withKatex(fn) {
  if (window.katex) return fn(window.katex);
  if (!katexCss) {
    katexCss = true;
    const l = document.createElement("link");
    l.rel = "stylesheet";
    l.href = vendorUrl("katex/katex.min.css");
    document.head.appendChild(l);
  }
  loadScript(vendorUrl("katex/katex.min.js")).then(
    () => fn(window.katex),
    (e) => console.warn("lmgw: KaTeX unavailable:", e.message)
  );
}

// pulldown-cmark emits <span class="math math-inline|math-display">TeX</span>.
// The markdown is re-parsed into fresh innerHTML on every update, so each pass
// starts from the TeX source again; rendering is idempotent by construction.
function renderMath(root) {
  const spans = root.querySelectorAll(".math-inline, .math-display");
  if (!spans.length) return;
  withKatex((katex) => {
    // The library may have landed after this pass's DOM was replaced.
    if (!root.isConnected) return;
    for (const el of root.querySelectorAll(".math-inline, .math-display")) {
      if (el.dataset.mathDone) continue;
      const tex = el.textContent || "";
      try {
        katex.render(tex, el, {
          displayMode: el.classList.contains("math-display"),
          throwOnError: false,
          // Explicit, not defaults: model output is untrusted (no \href /
          // \includegraphics), and macro expansion is bounded.
          trust: false,
          maxExpand: 1000,
          // A limit in em. KaTeX 0.18 *caps* a larger size at this value (it
          // does not raise an error), so \rule{2000em}{20000em} becomes a
          // 50em box, not a 300k px scroll area.
          maxSize: 50,
        });
      } catch (_) {}
      el.dataset.mathDone = "1";
    }
  });
}

// ---- mermaid -------------------------------------------------------------

// Token → mermaid themeVariables, read at render time so a theme switch is
// followed. `base` is the only theme that honours themeVariables.
function mermaidVars() {
  const cs = getComputedStyle(document.documentElement);
  const v = (n) => cs.getPropertyValue(n).trim();
  return {
    darkMode: cs.colorScheme.includes("dark"),
    background: v("--bg1"),
    fontFamily: v("--sans") || "sans-serif",
    primaryColor: v("--surface-hi"),
    primaryTextColor: v("--text"),
    primaryBorderColor: v("--accent"),
    secondaryColor: v("--surface"),
    tertiaryColor: v("--bg0"),
    lineColor: v("--text-2"),
    textColor: v("--text"),
    mainBkg: v("--surface-hi"),
    nodeBorder: v("--accent"),
    clusterBkg: v("--surface"),
    clusterBorder: v("--border-strong"),
    edgeLabelBackground: v("--surface"),
    titleColor: v("--text"),
    noteBkgColor: v("--surface"),
    noteTextColor: v("--text"),
    noteBorderColor: v("--border-strong"),
    actorBkg: v("--surface-hi"),
    actorBorder: v("--accent"),
    actorTextColor: v("--text"),
    signalColor: v("--text-2"),
    signalTextColor: v("--text"),
    labelBoxBkgColor: v("--surface"),
    labelBoxBorderColor: v("--border-strong"),
    labelTextColor: v("--text"),
    errorBkgColor: v("--err-bg"),
    errorTextColor: v("--err"),
  };
}

// ---- diagram colours -----------------------------------------------------

// Models copy Mermaid's documentation palette (fill:#f9f, fill:#bbf,
// stroke:#333, color:#fff): pastels drawn for a white page, which override the
// themeVariables above. Every colour a source sets — style / classDef /
// linkStyle declarations, a sequence diagram's `rect` and `box` — is moved onto
// the app palette instead: a hue picks the chart or status token named for it,
// a fill becomes a tint of that over --surface, a stroke the token itself, text
// is always --text, and a neutral (grey, black, white) becomes the matching
// surface / border / line token. Which parts the model told apart stays told
// apart; the pastel goes. Only the rendered copy changes — Code and Copy keep
// the source as written.

// Hue ranges (HSL degrees, upper bound) → the token standing for that colour
// name. Set by hand rather than "nearest token hue": the tokens sit off their
// names' centres (--c1 is a sky blue at 202°), so pure blue would land on
// purple and #f90 on yellow.
const HUE_SLOTS = [
  [15, "--err"], // red
  [42, "--c2"], // orange
  [72, "--amber"], // yellow
  [160, "--c5"], // green
  [194, "--c3"], // teal, cyan
  [244, "--c1"], // blue
  [297, "--c4"], // purple
  [345, "--c6"], // pink, magenta
  [360, "--err"],
];
const KEEP_COLOR = /^(none|transparent|inherit|initial|unset|currentcolor)$/i;
const COLOR_ROLE = {
  fill: "fill", background: "fill", "background-color": "fill",
  stroke: "stroke", "border-color": "stroke",
  color: "text",
};

let colorCtx;
// Any CSS colour → [r, g, b, a], or null. The canvas does the parsing, so
// names, #rgb(a), rgb() and hsl() all work; an invalid value leaves fillStyle
// alone, which two different starting values expose.
function parseColor(s) {
  colorCtx ??= document.createElement("canvas").getContext("2d");
  const read = (start) => {
    colorCtx.fillStyle = start;
    colorCtx.fillStyle = s;
    return colorCtx.fillStyle;
  };
  const v = read("#000000");
  if (v !== read("#ffffff")) return null;
  if (v[0] === "#") {
    return [1, 3, 5].map((i) => parseInt(v.slice(i, i + 2), 16)).concat(1);
  }
  const n = v.match(/[\d.]+/g).map(Number);
  return [n[0], n[1], n[2], n[3] ?? 1];
}

const hex = (c) =>
  "#" + c.slice(0, 3).map((x) => Math.round(x).toString(16).padStart(2, "0")).join("");
const rgb = (c) => "rgb(" + c.slice(0, 3).map(Math.round).join(", ") + ")";

function hueOf([r, g, b]) {
  const max = Math.max(r, g, b);
  const c = max - Math.min(r, g, b);
  if (!c) return 0;
  const h = max === r ? (g - b) / c : max === g ? (b - r) / c + 2 : (r - g) / c + 4;
  return (h * 60 + 360) % 360;
}

// Read at render time, like mermaidVars, so a theme switch is followed.
function diagramPalette() {
  const cs = getComputedStyle(document.documentElement);
  const v = (n) => parseColor(cs.getPropertyValue(n).trim()) || [128, 128, 128, 1];
  return {
    // Strong enough to tell hues apart, faint enough for --text on top.
    tint: cs.colorScheme.includes("dark") ? 0.24 : 0.16,
    surface: v("--surface"),
    fill: v("--surface-hi"),
    border: v("--border-strong"),
    line: v("--text-2"),
    text: v("--text"),
    slots: HUE_SLOTS.map(([upTo, n]) => [upTo, v(n)]),
  };
}

// One colour value → its palette replacement as [r, g, b], or null to leave it
// as written.
function mapColor(value, role, pal) {
  const s = value.trim();
  if (KEEP_COLOR.test(s)) return null;
  const c = parseColor(s);
  if (!c || c[3] === 0) return null;
  if (role === "text") return pal.text;
  // Below ~6% chroma there is no hue worth keeping (#333, #eee, black, white).
  if (Math.max(...c.slice(0, 3)) - Math.min(...c.slice(0, 3)) < 16) {
    return role === "fill" ? pal.fill : role === "line" ? pal.line : pal.border;
  }
  const h = hueOf(c);
  const slot = pal.slots.find(([upTo]) => h < upTo)[1];
  if (role !== "fill") return slot;
  return slot.map((x, i) => x * pal.tint + pal.surface[i] * (1 - pal.tint));
}

// A declaration list ("fill:#f9f,stroke:#333,stroke-width:2px"), split on the
// commas outside parentheses (rgb(…) keeps its own; `\,` is Mermaid's escape).
function recolorDecls(decls, link, pal) {
  const parts = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < decls.length; i++) {
    const ch = decls[i];
    if (ch === "(") depth++;
    else if (ch === ")") depth = Math.max(0, depth - 1);
    else if (ch === "," && !depth && decls[i - 1] !== "\\") {
      parts.push(decls.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(decls.slice(start));
  return parts
    .map((d) => {
      const m = /^(\s*)([\w-]+)(\s*:\s*)(.*?)(\s*!important)?(\s*)$/i.exec(d);
      let role = m && COLOR_ROLE[m[2].toLowerCase()];
      if (!role) return d;
      if (link && role === "stroke") role = "line";
      const out = mapColor(m[4].replace(/\\,/g, ","), role, pal);
      return out ? m[1] + m[2] + m[3] + hex(out) + (m[5] || "") + m[6] : d;
    })
    .join(",");
}

// Statements start a line or follow a `;` (`graph TD; A-->B; style A …`). A
// sequence diagram's rect/box line ends at `#`, so those get rgb(), not hex.
const STYLE_STMT = /(^|[;\n])([ \t]*)(style|classDef|linkStyle)([ \t]+\S+[ \t]+)([^;\n]*)/g;
const RECT_STMT = /(^|[;\n])([ \t]*rect[ \t]+)([^;\n]*)/g;
const BOX_STMT = /(^|\n)([ \t]*box[ \t]+)(rgba?\([^)\n]*\)|hsla?\([^)\n]*\)|#[0-9a-f]+|[a-z]+)/gi;

function recolorSource(src, pal) {
  return src
    .replace(STYLE_STMT, (_, lead, ind, kw, ids, decls) =>
      lead + ind + kw + ids + recolorDecls(decls, kw === "linkStyle", pal))
    .replace(RECT_STMT, (all, lead, kw, color) => {
      const out = mapColor(color, "fill", pal);
      return out ? lead + kw + rgb(out) : all;
    })
    .replace(BOX_STMT, (all, lead, kw, color) => {
      const out = mapColor(color, "fill", pal);
      return out ? lead + kw + rgb(out) : all;
    });
}

// One render at a time (mermaid keeps global state), and identical source under
// the same theme is not laid out again on the next settle.
let mermaidQueue = Promise.resolve();
// A cache bound (least recently used out), not a content cap: an evicted
// diagram is simply laid out again.
const SVG_CACHE_MAX = 64;
const svgCache = new Map();
let mermaidSeq = 0;

function cacheGet(key) {
  if (!svgCache.has(key)) return undefined;
  const v = svgCache.get(key);
  svgCache.delete(key);
  svgCache.set(key, v);
  return v;
}
function cacheSet(key, v) {
  svgCache.set(key, v);
  while (svgCache.size > SVG_CACHE_MAX) {
    svgCache.delete(svgCache.keys().next().value);
  }
}

// UX hint only, NOT a security control. A source that obviously names an
// external image is refused up front with a note instead of rendering a
// diagram with a hole in it. Anything this misses (CSS escapes, quoted keys,
// image-set(), …) is handled by the two real controls: the allowlist
// sanitiser below, and the dashboard's Content-Security-Policy (img-src /
// media-src / font-src / style-src), which stops the browser fetching
// anything remote whatever ends up in the DOM.
const MERMAID_EXTERNAL =
  /@\{[^}]*\bimg\s*:|url\s*\(|<\s*(img|image|iframe|object|embed|link|video|audio|source)\b|@import|\bthemeCSS\b|\bimage\s*:\s*["']?\s*(https?:)?\/\//i;
class DiagramRefused extends Error {}
const REFUSED_NOTE = "diagram references external images — not rendered";

// Allowlist sanitiser: DOMPurify's SVG profile (vendored, vendor/dompurify/),
// not a blocklist. <style> is kept (mermaid's theming needs it; the CSP stops
// CSS from loading anything). Element types that can pull in or execute other
// content are forbidden, links may only point at "#…", and every on* attribute
// goes. Returns the SVG element itself; the caller imports the node rather than
// re-serialising it through innerHTML.
let purifyReady = false;
function purifier() {
  const P = window.DOMPurify;
  if (!purifyReady) {
    purifyReady = true;
    P.addHook("uponSanitizeAttribute", (_node, data) => {
      const n = data.attrName.toLowerCase();
      if (n.startsWith("on")) {
        data.keepAttr = false;
      } else if (n === "href" || n === "xlink:href") {
        if (!/^\s*#/.test(data.attrValue)) data.keepAttr = false;
      } else if (/url\s*\(\s*(?!["']?#)|image-set|@import/i.test(data.attrValue)) {
        data.keepAttr = false;
      }
    });
  }
  return P;
}

function sanitizeSvg(svg) {
  const body = purifier().sanitize(svg, {
    USE_PROFILES: { svg: true, svgFilters: true },
    ADD_TAGS: ["style"],
    FORBID_TAGS: [
      "use", "set", "animate", "animateMotion", "animateTransform", "feImage",
      "foreignObject", "script", "image", "img", "iframe", "object", "embed",
      "video", "audio", "link",
    ],
    RETURN_DOM: true,
  });
  const root = body.querySelector("svg");
  if (!root) throw new Error("diagram produced invalid SVG");
  for (const st of [...root.querySelectorAll("style")]) {
    if (/@import|image-set|url\s*\(\s*(?!["']?#)/i.test(st.textContent || "")) st.remove();
  }
  return root;
}

function mermaidInit(vars) {
  window.mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    // Mermaid's default `secure` list, plus every key that can carry CSS or a
    // font/URL, so a %%{init: …}%% directive in the source cannot set them.
    secure: [
      "secure", "securityLevel", "startOnLoad", "maxTextSize",
      "suppressErrorRendering", "maxEdges",
      "theme", "themeCSS", "themeVariables", "fontFamily", "altFontFamily",
      "htmlLabels",
    ],
    // Text labels are SVG <text>, not HTML in a foreignObject.
    htmlLabels: false,
    flowchart: { htmlLabels: false },
    theme: "base",
    themeVariables: vars,
  });
}

async function mermaidSvg(src) {
  if (MERMAID_EXTERNAL.test(src)) throw new DiagramRefused(REFUSED_NOTE);
  await loadScript(vendorUrl("dompurify/purify.min.js"));
  await loadScript(vendorUrl("mermaid/mermaid.min.js"));
  const vars = mermaidVars();
  const themed = recolorSource(src, diagramPalette());
  const key = JSON.stringify(vars) + "\n" + themed;
  const hit = cacheGet(key);
  if (hit !== undefined) return hit;
  mermaidInit(vars);
  const id = "lmgw-mmd-" + ++mermaidSeq;
  try {
    const { svg } = await window.mermaid.render(id, themed);
    const clean = sanitizeSvg(svg);
    cacheSet(key, clean);
    return clean;
  } finally {
    // mermaid leaves its scratch/error nodes in <body> on failure.
    document.getElementById(id)?.remove();
    document.getElementById("d" + id)?.remove();
  }
}

function diagramWrap(view, codeWrap, src) {
  const wrap = document.createElement("div");
  wrap.className = "mermaid-wrap";
  const bar = document.createElement("div");
  bar.className = "code-tools";
  const tag = document.createElement("span");
  tag.className = "lang";
  tag.textContent = "mermaid";
  bar.appendChild(tag);
  const toggle = document.createElement("button");
  toggle.title = "Show the diagram source";
  toggle.textContent = "Code";
  toggle.addEventListener("click", () => {
    const showCode = !wrap.classList.contains("show-code");
    wrap.classList.toggle("show-code", showCode);
    toggle.textContent = showCode ? "Diagram" : "Code";
  });
  bar.appendChild(toggle);
  const copy = document.createElement("button");
  copy.title = "Copy diagram source";
  copy.innerHTML = `${ICON_COPY}<span>Copy</span>`;
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(src);
      copy.classList.add("ok");
      copy.innerHTML = `${ICON_CHECK}<span>Copied</span>`;
      setTimeout(() => {
        copy.classList.remove("ok");
        copy.innerHTML = `${ICON_COPY}<span>Copy</span>`;
      }, 1200);
    } catch (_) {}
  });
  bar.appendChild(copy);
  wrap.append(view, bar);
  codeWrap.classList.add("mermaid-src");
  return wrap;
}

function renderDiagrams(root) {
  for (const code of root.querySelectorAll("pre > code.language-mermaid")) {
    const pre = code.parentElement;
    const codeWrap = pre.parentElement?.classList.contains("code-wrap")
      ? pre.parentElement
      : pre;
    if (codeWrap.dataset.mermaid) continue;
    codeWrap.dataset.mermaid = "1";
    const src = code.textContent || "";
    mermaidQueue = mermaidQueue.then(async () => {
      try {
        const svg = await mermaidSvg(src);
        // The message may have been re-rendered while mermaid worked.
        if (!codeWrap.isConnected) return;
        const view = document.createElement("div");
        view.className = "mermaid-view";
        view.appendChild(document.importNode(svg, true));
        const wrap = diagramWrap(view, codeWrap, src);
        codeWrap.replaceWith(wrap);
        wrap.appendChild(codeWrap);
      } catch (e) {
        if (!codeWrap.isConnected) return;
        const err = document.createElement("div");
        err.className = "mermaid-error";
        err.textContent =
          e instanceof DiagramRefused
            ? e.message.charAt(0).toUpperCase() + e.message.slice(1)
            : "Diagram error: " + (e?.message || e);
        codeWrap.after(err);
      }
    });
  }
}

// This module is a plain <script type="module">, so it normally lands well
// before the wasm bundle finishes instantiating. If it ever loses that race,
// decorate whatever the first render already put on the page (nothing can be
// streaming that early).
for (const el of document.querySelectorAll(".md")) {
  window.lmgwDecorateCode(el, true);
}
