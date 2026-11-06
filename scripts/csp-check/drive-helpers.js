// Helpers the CSP check's drives call (scripts/drive/chat-csp*.json), loaded
// with a {"script": …} step after every load. Evaluated over DevTools, so the
// page's policy does not apply to this file; what it drives does (its
// fetches are the page's connect-src too, so it fetches nothing but 'self').
// Its own requests carry x-lmgw-test-driver: they are the drive's, not the
// page's, and the proxy's relay check passes them by.
(() => {
  const j = async (u, b) =>
    (
      await fetch(u, {
        method: b === undefined ? "GET" : "POST",
        headers: { "content-type": "application/json", "x-lmgw-test-driver": "1" },
        body: b === undefined ? undefined : JSON.stringify(b),
      })
    ).json();
  // The proxy's control routes (scripts/csp-check/proxy.py).
  const proxy = async (name, b) =>
    (
      await fetch("/__csp-check/" + name, {
        method: b === undefined ? "GET" : "POST",
        body: b === undefined ? undefined : JSON.stringify(b),
      })
    ).json();
  // Code, math, a diagram and a previewable block: what the Chat's scripts
  // (codeblocks.js and its libraries) decorate.
  const PROBE = [
    "csp probe",
    "",
    "```python",
    "def f(x):",
    "    return x * 2",
    "```",
    "",
    "Euler: $e^{i\\pi}+1=0$",
    "",
    "```mermaid",
    "graph TD; A[Start] --> B[Done]",
    "```",
    "",
    "```html",
    '<p id="pv">preview body</p>',
    // In the relay drive's preview page it tries to replace the code; the
    // page must ignore it (preview-host.js).
    '<script src="/__csp-check/preview-intruder.js"></script>',
    "```",
    "",
  ].join("\n");
  // The previewed page's intruder (preview-intruder.js) says when it ran.
  if (!window.__intruderHooked) {
    window.__intruderHooked = true;
    window.addEventListener("message", (e) => {
      if (e.data && e.data.type === "csp-check-intruder-ran") window.__intruderRan = true;
    });
  }
  window.__csp = {
    j,
    proxy,
    PROBE,
    // The relayed requests the proxy saw that match `method` and the path
    // pattern `re`.
    async relayed(method, re) {
      const log = await proxy("log");
      return log.relayed.filter((r) => r.method === method && new RegExp(re).test(r.path));
    },
    // A 1x1 PNG.
    PNG: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
    // A new thread on `alias`, opened.
    async open(alias) {
      const t = await j("/chat/api/threads", { model_alias: alias, kind: "chat" });
      setTimeout(() => location.assign("/chat?t=" + t.id), 50);
      return t.id;
    },
    compose(text) {
      const ta = document.querySelector(".composer-input");
      ta.value = text;
      ta.dispatchEvent(new Event("input", { bubbles: true }));
      return "composed";
    },
    send() {
      document.querySelector(".composer-send[aria-label=Send]").click();
      return "sent";
    },
    // A file picked in the composer's file input.
    up(name, b64, type) {
      const b = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      const dt = new DataTransfer();
      dt.items.add(new File([b], name, { type }));
      const i = document.querySelector(".composer input[type=file]");
      i.files = dt.files;
      i.dispatchEvent(new Event("change", { bubbles: true }));
      return "picked " + name;
    },
    // Downloads are recorded, not started: a headless browser has no
    // Downloads UI. The blobs object URLs are made of are kept, so a check
    // reads a download's bytes without fetching a blob: URL.
    hookDownloads() {
      window.__downloads = [];
      window.__blobs = {};
      const make = URL.createObjectURL;
      URL.createObjectURL = function (b) {
        const u = make.call(URL, b);
        window.__blobs[u] = b;
        return u;
      };
      const orig = HTMLAnchorElement.prototype.click;
      HTMLAnchorElement.prototype.click = function () {
        if (this.hasAttribute("download")) {
          window.__downloads.push({
            href: this.getAttribute("href"),
            name: this.getAttribute("download"),
          });
          return;
        }
        return orig.call(this);
      };
      return "hooked";
    },
    // The newest reply's rendered markdown.
    reply() {
      const r = [...document.querySelectorAll(".chat-col .msg-assistant .md")];
      return r[r.length - 1];
    },
    menuItem(text) {
      const it = [...document.querySelectorAll(".menu-pop .menu-item")].find((e) =>
        e.textContent.includes(text),
      );
      if (!it) return "no menu item " + text;
      it.click();
      return "clicked " + text;
    },
    // Whether a picture's object URL still answers: loaded as an image
    // (img-src blob:), not fetched (connect-src has no blob:).
    alive(url) {
      return new Promise((done) => {
        const i = new Image();
        i.onload = () => done("alive");
        i.onerror = () => done("revoked");
        i.src = url;
      });
    },
  };
  return "helpers";
})();
