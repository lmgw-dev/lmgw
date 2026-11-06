// A previewed page that tries to hand the host's preview page code of its own
// (scripts/drive/chat-csp-relay.json): the preview page takes code only from
// the page that framed it (preview-host.js), so this is ignored. It tells the
// drive that it ran, so the check is not vacuous.
parent.postMessage({ type: "lmgw-preview", code: "<p>intruder</p>", lang: "html" }, "*");
top.postMessage({ type: "csp-check-intruder-ran" }, "*");
