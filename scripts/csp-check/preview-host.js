// The test host's preview page (preview-host.html): the Chat posts
// {type: "lmgw-preview", code, lang} once the page loaded, and again when the
// open preview's code changes. Listening starts before load, as the Chat
// posts on the frame's load event. Only the page that framed this one may
// hand it code (`ev.source === parent`): another window holding a reference
// to it (the previewed page itself, a popup) may not. The sidebar's page
// (WP8) checks the same.
window.addEventListener("message", (ev) => {
  if (ev.source !== window.parent || window.parent === window) return;
  const m = ev.data;
  if (!m || m.type !== "lmgw-preview" || typeof m.code !== "string") return;
  let frame = document.querySelector("iframe");
  if (!frame) {
    frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts");
    frame.style.cssText = "display:block;border:0;width:100%;height:100vh";
    document.body.appendChild(frame);
  }
  frame.srcdoc = m.code;
  document.body.dataset.lang = m.lang || "";
  document.body.dataset.received = String(Number(document.body.dataset.received || 0) + 1);
});
