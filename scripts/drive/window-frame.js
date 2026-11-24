// The app window's framed state in headless Chrome, for shots that show the window as the desktop
// does (scripts/readme-shots.sh): the shell's own frame (src-tauri/src/window_chrome.rs) on a
// focused window that is neither maximized nor tiled, the window buttons minimize, maximize and
// close at the end of the header (GTK's default decoration layout, KDE's too).
//
// The shell's page script (src-tauri/src/window_chrome/page.js) is what puts that state on the
// page: the html classes, --csd-radius, window.lmgwWindow and the `lmgw-window` event that
// crate::frame reads to draw the buttons. It asks the shell over Tauri's IPC, which a browser does
// not have. So the drive loads this file twice, around page.js:
//
//   --init-script scripts/drive/window-frame.js --init-script src-tauri/src/window_chrome/page.js
//   --init-script scripts/drive/window-frame.js
//
// The first run puts a bridge in place whose only answer is the `window_chrome` command's state
// (every other call resolves to null), and page.js takes it; the second run removes it again, so
// the rest of the page (boot.js, desktop.js, the interface scale, the voice output routing, the
// machine check) sees a browser, as the earlier shots did, and nothing else is faked. html.tauri
// (the app window's scrollbars) is set here, as desktop.js does in the app.
//
// The radius and zoom are the app's at its default 125 % interface scale: window_chrome.rs
// DEFAULT_RADIUS (17.5 window px), which page.js turns into --csd-radius 14px, the page's own
// radius (frame.rs RADIUS). Run the drive at --dsf 1.25 so a CSS pixel is 1.25 shot pixels, as in
// the window.
(() => {
  "use strict";
  const w = window;
  if (w.__lmgwFrameFake) {
    if (w.__TAURI_INTERNALS__ === w.__lmgwFrameFake) delete w.__TAURI_INTERNALS__;
    return;
  }
  if (w.__TAURI_INTERNALS__) return; // a real shell: leave it alone
  const state = {
    csd: true,
    maximized: false,
    fullscreen: false,
    tiled: false,
    inactive: false,
    radius: 17.5,
    zoom: 1.25,
    layout: { start: [], end: ["minimize", "maximize", "close"] },
  };
  w.__lmgwFrameFake = {
    invoke(cmd, args) {
      const action = args && args.action;
      if (cmd === "window_chrome" && action && action.do === "state") return Promise.resolve(state);
      return Promise.resolve(null);
    },
  };
  w.__TAURI_INTERNALS__ = w.__lmgwFrameFake;
  // <html> may not exist yet this early (page.js waits for DOMContentLoaded the same way).
  const mark = () => document.documentElement && document.documentElement.classList.add("tauri");
  mark();
  document.addEventListener("DOMContentLoaded", mark);
})();
