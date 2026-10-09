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
    // Served from the gateway's root: no base; a `401 session_required` raises the login gate.
    lmgw_ui_kit::http::configure(lmgw_ui_kit::http::Config {
        base: String::new(),
        on_unauthorized: Some(session::lock),
    });
    leptos::mount::mount_to_body(app::App);
}
