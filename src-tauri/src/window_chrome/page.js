// The page's end of the CSD frame (src-tauri/src/window_chrome.rs, module
// docs): the window's state as classes on <html> and as `lmgwWindow.state`,
// headers marked `data-window-header` that move the window, buttons marked
// `data-window-button`, and `lmgwWindow.minimize()` and friends. Injected by
// the shell at document start, in the main frame, only while it draws the
// frame itself (the default on Linux; not with LMGW_WINDOW=kwin).
(function () {
  "use strict";
  const internals = window.__TAURI_INTERNALS__;
  if (!internals || window.lmgwWindow) return;

  const call = (action) => internals.invoke("window_chrome", { action });
  const quiet = (p) => p.catch((e) => console.warn("lmgw window:", e));
  const CLASSES = ["csd", "maximized", "fullscreen", "tiled", "inactive"];

  const api = {
    state: null,
    minimize: () => quiet(call({ do: "minimize" })),
    maximize: () => quiet(call({ do: "maximize" })),
    close: () => quiet(call({ do: "close" })),
    menu: () => quiet(call({ do: "menu" })),
    radius: (px) => quiet(call({ do: "radius", px })),
  };
  window.lmgwWindow = api;

  function apply(s) {
    if (!s) return;
    api.state = s;
    const el = document.documentElement;
    if (el) {
      for (const c of CLASSES) el.classList.toggle(c, !!s[c]);
      el.style.setProperty("--csd-radius", s.radius / (s.zoom || 1) + "px");
    }
    window.dispatchEvent(new CustomEvent("lmgw-window", { detail: s }));
  }
  // The shell pushes every change here.
  window.__lmgwChrome = apply;
  quiet(call({ do: "state" }).then(apply));
  document.addEventListener("DOMContentLoaded", () => apply(api.state));

  // --- headers --------------------------------------------------------------
  // A press on a header, not on a control in it: a drag past a few pixels
  // moves the window (GTK's titlebar waits for the drag threshold too), a
  // double, middle or right click does what GTK's titlebar settings say.
  const CONTROL =
    "button, a, input, select, textarea, label, [contenteditable], [role=button], " +
    "[data-window-button], [data-no-drag], " +
    // Popovers, menus and dialogs rendered in place inside a header bar are
    // not the bar: a scrollbar drag or a text selection in them stays theirs.
    "[popover], dialog, [role=menu], [role=listbox], [role=dialog]";
  function onHeader(target) {
    if (!(target instanceof Element)) return false;
    const header = target.closest("[data-window-header]");
    if (!header) return false;
    const control = target.closest(CONTROL);
    return !(control && header.contains(control));
  }
  let pressed = null;
  document.addEventListener(
    "mousedown",
    (e) => {
      if (!onHeader(e.target)) return;
      if (e.button === 0) {
        e.preventDefault(); // no text selection from the header
        pressed = null;
        if (e.detail === 2) quiet(call({ do: "click", button: 1, count: 2 }));
        else if (e.detail === 1) pressed = { x: e.clientX, y: e.clientY };
      } else if (e.button === 1) {
        e.preventDefault();
        quiet(call({ do: "click", button: 2, count: 1 }));
      }
    },
    true,
  );
  document.addEventListener(
    "mousemove",
    (e) => {
      if (!pressed) return;
      if (!(e.buttons & 1)) {
        pressed = null;
        return;
      }
      if (Math.hypot(e.clientX - pressed.x, e.clientY - pressed.y) < 4) return;
      pressed = null;
      quiet(call({ do: "drag" }));
    },
    true,
  );
  document.addEventListener("mouseup", () => (pressed = null), true);
  document.addEventListener(
    "contextmenu",
    (e) => {
      if (!onHeader(e.target)) return;
      e.preventDefault();
      quiet(call({ do: "click", button: 3, count: 1 }));
    },
    true,
  );

  // --- buttons --------------------------------------------------------------
  document.addEventListener("click", (e) => {
    const b = e.target instanceof Element && e.target.closest("[data-window-button]");
    if (!b) return;
    const what = b.dataset.windowButton;
    if (what === "minimize" || what === "maximize" || what === "close") {
      e.preventDefault();
      api[what]();
    }
  });

  // --- fallback strip -------------------------------------------------------
  // A page with no visible header gets a thin strip on top: a drag area and
  // the window buttons in the desktop's order, so the window stays usable
  // (a page that has not mounted yet, one that failed to, or a route that
  // draws none). It also rounds the page's corners, which the shell cannot
  // clip (window_chrome/linux.rs): a background image on <html> keeps the
  // body's colour off the canvas, and a clip-path on <html> rounds
  // everything, fixed elements too.
  const ICONS = {
    minimize: '<path d="M6 12.5h12"/>',
    maximize: '<rect x="6.5" y="6.5" width="11" height="11" rx="1.5"/>',
    close: '<path d="M7 7l10 10M17 7 7 17"/>',
  };
  const STRIP_CSS = `
html.csd-strip body { margin-top: 30px !important; height: calc(100% - 30px) !important; }
#lmgw-csd-strip { position: fixed; top: 0; left: 0; right: 0; height: 30px; z-index: 2147483647;
  display: flex; align-items: center; gap: 8px; padding: 0 8px; box-sizing: border-box;
  background: var(--desk-bg0, var(--bg0, #2a2d31)); color: var(--text, #e9ebee);
  font: 12px system-ui, sans-serif; user-select: none; cursor: default; }
#lmgw-csd-strip .grow { flex: 1; text-align: center; opacity: .7; }
#lmgw-csd-strip .btns { display: flex; gap: 8px; }
#lmgw-csd-strip button { width: 20px; height: 20px; border-radius: 50%; border: 0; padding: 0;
  background: rgba(127,127,127,.2); color: inherit; display: grid; place-items: center; }
#lmgw-csd-strip button:hover { background: rgba(127,127,127,.35); }
#lmgw-csd-strip svg { width: 12px; height: 12px; stroke: currentColor; fill: none; stroke-width: 2.2;
  stroke-linecap: round; stroke-linejoin: round; }
html.inactive #lmgw-csd-strip { opacity: .75; }
html.csd-strip:not(.maximized):not(.fullscreen):not(.tiled) {
  background: linear-gradient(transparent, transparent) !important;
  clip-path: inset(0 round var(--csd-radius)); }`;
  function buttons(names) {
    const box = document.createElement("div");
    box.className = "btns";
    for (const n of names) {
      if (!ICONS[n]) continue;
      const b = document.createElement("button");
      b.dataset.windowButton = n;
      b.title = n[0].toUpperCase() + n.slice(1);
      b.innerHTML = `<svg viewBox="0 0 24 24">${ICONS[n]}</svg>`;
      box.append(b);
    }
    return box;
  }
  // Rebuilt only when the buttons' layout or the title changed: a rebuild
  // between a press and its click (every state push, such as the focus
  // change the press itself causes) replaces the button and loses the click.
  function fillStrip(strip) {
    const key = JSON.stringify(api.state && api.state.layout) + "\n" + document.title;
    if (strip.dataset.key === key) return;
    strip.dataset.key = key;
    const layout = (api.state && api.state.layout) || { start: [], end: ["minimize", "maximize", "close"] };
    const grow = document.createElement("div");
    grow.className = "grow";
    grow.textContent = document.title || "lmgw";
    strip.replaceChildren(buttons(layout.start), grow, buttons(layout.end));
  }
  // The strip stands in for a header while the page has none: from the
  // document's start (the dashboard's WASM draws its headers after it
  // loads, and a page whose WASM fails to mount never gets any) until a
  // visible header exists, and again if the last one goes. Watched with a
  // MutationObserver, checked at most once per frame.
  let stripEl = null;
  let styleEl = null;
  const hasHeader = () =>
    [...document.querySelectorAll("[data-window-header]")].some(
      (h) => h !== stripEl && !h.closest("[hidden]"),
    );
  function showStrip() {
    if (stripEl) return;
    if (!styleEl) {
      styleEl = document.createElement("style");
      styleEl.textContent = STRIP_CSS;
      document.head.append(styleEl);
    }
    stripEl = document.createElement("div");
    stripEl.id = "lmgw-csd-strip";
    stripEl.setAttribute("data-window-header", "");
    fillStrip(stripEl);
    document.body.append(stripEl);
    document.documentElement.classList.add("csd-strip");
  }
  function hideStrip() {
    if (!stripEl) return;
    stripEl.remove();
    stripEl = null;
    document.documentElement.classList.remove("csd-strip");
  }
  function sync() {
    if (!document.body) return;
    if (hasHeader()) hideStrip();
    else showStrip();
  }
  window.addEventListener("lmgw-window", () => stripEl && fillStrip(stripEl));
  function watch() {
    sync();
    let queued = false;
    new MutationObserver(() => {
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        sync();
      });
    }).observe(document.body, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: ["hidden", "data-window-header"],
    });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", watch);
  else watch();
})();
