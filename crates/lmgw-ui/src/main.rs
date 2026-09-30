mod api;
mod app;
mod backends_api;
mod bench_api;
mod catalog;
mod charts;
mod fmt;
mod live;
mod model_ops;
mod ops_state;
mod pages;
mod prefs;
mod scope;
mod session;
mod shell;
mod ui_scale;
mod url_state;
mod widgets;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
    // Tell titlebar.js the window controls exist now (it wires them lazily
    // because WASM mounts after the script runs).
    if let Ok(ev) = web_sys::CustomEvent::new("lmgw:mounted") {
        let _ = leptos::prelude::document().dispatch_event(&ev);
    }
}
