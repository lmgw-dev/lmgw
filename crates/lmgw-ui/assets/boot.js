// Before first paint: the theme and the interface scale. A classic script
// (not a module), so it runs while <head> is parsed, before anything is drawn.
// A file rather than inline script: a page under `script-src 'self'` (an
// extension page, chat-plane design "WP3a: the transport seam") refuses
// inline script.
(function () {
  "use strict";
  // Theme; "dark" unless explicitly switched in Settings.
  try {
    var t = localStorage.getItem("lmgw-theme");
    if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  } catch (_) {}
  // Interface scale, likewise before the app renders anything: real webview
  // zoom, so a restart comes up scaled instead of snapping when WASM mounts.
  // Browsers have their own zoom and no __TAURI_INTERNALS__ — see ui_scale.rs,
  // whose DEFAULT (125) is the fallback here too.
  try {
    var api = window.__TAURI_INTERNALS__;
    var s = parseInt(localStorage.getItem("lmgw-ui-scale"), 10);
    if (!(s >= 50 && s <= 300)) s = 125;
    if (api) api.invoke("plugin:webview|set_webview_zoom", { value: s / 100 });
  } catch (_) {}
})();
