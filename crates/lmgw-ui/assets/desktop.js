// Desktop shell: when the UI runs inside the lmgw Tauri webview, mark the page
// so app.css can follow the desktop (its fonts), give the clickable parts the
// desktop's arrow cursor, and tell the shell when the app has drawn itself.
// No-op in ordinary LAN browsers.
//
// The window is decorated: KWin draws the title bar, the window buttons and
// the resize borders, so nothing here wires window controls, edge resizing or
// a drag region. The right-click menu is trimmed in the shell
// (src-tauri/src/context_menu.rs), on WebKit's own menu.
(function () {
  "use strict";
  const internals = window.__TAURI_INTERNALS__;
  if (!internals) return;

  document.documentElement.classList.add("tauri");

  // --- first show ---------------------------------------------------------
  // The shell keeps the window hidden until the app has mounted, so it opens
  // drawn instead of as an empty pane while the WASM loads
  // (src-tauri/src/reveal.rs). Trunk's boot script dispatches this once
  // `init` has run the app's main(), which mounts the UI synchronously.
  window.addEventListener(
    "TrunkApplicationStarted",
    () => {
      internals
        .invoke("plugin:event|emit", { event: "lmgw:mounted", payload: null })
        .catch(() => {}); // the shell shows the window on its own then
    },
    { once: true },
  );

  // --- desktop cursor -----------------------------------------------------
  // A desktop app keeps the arrow over its buttons, rows and tabs; the
  // pointing hand is a web-page convention. app.css sets it in many rules,
  // so the shell rewrites those declarations once the stylesheets are in,
  // and the browser view keeps them. Links in running text keep the hand
  // from the browser's own stylesheet, as they do in desktop apps.
  function walk(rules) {
    for (const r of rules) {
      if (r.style && r.style.cursor === "pointer") r.style.cursor = "default";
      // @media, @container and @supports blocks nest their rules.
      if (r.cssRules) walk(r.cssRules);
    }
  }
  function arrowCursors() {
    for (const sheet of document.styleSheets) {
      let rules;
      try {
        rules = sheet.cssRules;
      } catch (_) {
        continue; // a cross-origin sheet: not ours
      }
      walk(rules);
    }
  }
  if (document.readyState === "complete") arrowCursors();
  else window.addEventListener("load", arrowCursors);
})();
