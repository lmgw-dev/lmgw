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
}
