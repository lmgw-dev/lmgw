// The app's loader, as a file. Trunk writes it as an inline module script by
// default, which a page under `script-src 'self' 'wasm-unsafe-eval'` (an
// extension page, chat-plane design "WP3a: the transport seam") refuses.
// Trunk.toml's `pattern_script` leaves this build's file names on
// <meta name="lmgw-wasm"> instead, and this does what the inline one did:
// load the wasm-bindgen glue, run the app, and say so with the event the
// desktop shell waits for (desktop.js, `TrunkApplicationStarted`).
const meta = document.querySelector('meta[name="lmgw-wasm"]');
const bindings = await import(meta.dataset.js);
const wasm = await bindings.default({ module_or_path: meta.dataset.wasm });
window.wasmBindings = bindings;
dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm } }));
