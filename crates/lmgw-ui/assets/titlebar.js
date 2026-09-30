// Unified title bar: when the UI runs inside the lmgw Tauri webview, the
// titlebar acts as the window title bar. No-op in ordinary LAN browsers.
//
// Ported from the htmx UI's titlebar.js. Dragging itself is handled by Tauri's
// injected drag-region script via the data-tauri-drag-region attribute; this
// file adds the window control buttons and edge resizing (undecorated Linux
// windows have no native resize borders, so we start compositor resizes
// explicitly). Difference from the old version: the Leptos app renders the
// buttons after WASM init, so button wiring waits for the `lmgw:mounted`
// event dispatched from Rust instead of assuming the DOM is ready at load.
(function () {
  "use strict";
  const internals = window.__TAURI_INTERNALS__;
  if (!internals) return;
  const invoke = (cmd, args) => internals.invoke("plugin:window|" + cmd, args || {});

  document.documentElement.classList.add("tauri");

  // --- window control buttons -------------------------------------------
  function initButtons() {
    const controls = document.getElementById("win-controls");
    if (!controls || controls.dataset.wired) return;
    controls.dataset.wired = "1";
    controls.hidden = false;

    const minBtn = document.getElementById("win-min");
    const maxBtn = document.getElementById("win-max");
    const closeBtn = document.getElementById("win-close");

    async function syncMaxGlyph() {
      if (!maxBtn) return;
      try {
        const maximized = await invoke("is_maximized");
        maxBtn.textContent = maximized ? "❏" : "▢";
        maxBtn.title = maximized ? "Restore" : "Maximize";
      } catch (_) {
        /* permission missing — leave default glyph */
      }
    }

    if (minBtn) minBtn.addEventListener("click", () => invoke("minimize"));
    if (maxBtn)
      maxBtn.addEventListener("click", async () => {
        await invoke("toggle_maximize").catch(() => {});
        syncMaxGlyph();
      });
    // close() fires CloseRequested; the Rust handler hides to tray.
    if (closeBtn) closeBtn.addEventListener("click", () => invoke("close"));

    let resizeTimer;
    window.addEventListener("resize", () => {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(syncMaxGlyph, 150);
    });
    syncMaxGlyph();
  }
  document.addEventListener("lmgw:mounted", initButtons);
  initButtons(); // in case the app mounted before this script ran

  // --- edge resize (document-level, independent of app mount) -------------
  // The hit zone comes from the stylesheet's --resize-edge, which is also what
  // the scroll panes reserve as a gutter (app.css) so the strip and a native
  // scrollbar never land on the same pixels. One value, read once on first use
  // — by then the stylesheet is applied; the literal is only a fallback.
  let edgePx = 0;
  function edge() {
    if (!edgePx) {
      const v = parseFloat(
        getComputedStyle(document.documentElement).getPropertyValue("--resize-edge")
      );
      edgePx = v > 0 ? v : 9;
    }
    return edgePx;
  }

  function dirAt(x, y) {
    const EDGE = edge();
    const w = window.innerWidth;
    const h = window.innerHeight;
    const n = y <= EDGE;
    const s = y >= h - EDGE;
    const e = x >= w - EDGE;
    const wst = x <= EDGE;
    if (n && wst) return "NorthWest";
    if (n && e) return "NorthEast";
    if (s && wst) return "SouthWest";
    if (s && e) return "SouthEast";
    if (n) return "North";
    if (s) return "South";
    if (e) return "East";
    if (wst) return "West";
    return null;
  }

  const CURSORS = {
    North: "n-resize",
    South: "s-resize",
    East: "e-resize",
    West: "w-resize",
    NorthEast: "ne-resize",
    NorthWest: "nw-resize",
    SouthEast: "se-resize",
    SouthWest: "sw-resize",
  };

  let maximized = false;
  const trackMax = () =>
    invoke("is_maximized").then((m) => (maximized = m)).catch(() => {});
  trackMax();
  window.addEventListener("resize", trackMax);

  document.addEventListener("mousemove", (ev) => {
    const dir = maximized ? null : dirAt(ev.clientX, ev.clientY);
    document.documentElement.style.cursor = dir ? CURSORS[dir] : "";
  });

  document.addEventListener(
    "mousedown",
    (ev) => {
      if (ev.button !== 0 || maximized) return;
      const dir = dirAt(ev.clientX, ev.clientY);
      if (!dir) return;
      // Run in capture phase and stop propagation so the top edge wins over
      // the titlebar drag region.
      ev.preventDefault();
      ev.stopImmediatePropagation();
      invoke("start_resize_dragging", { value: dir });
    },
    true
  );
})();
