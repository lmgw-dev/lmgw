//! The design sample in the main window (debug builds only):
//! `LMGW_WINDOW_DEMO=1` loads `docs/design/ui-rebuild/modern-sample.html`
//! into the main window instead of the dashboard, so the new frame can be
//! felt with the page it was made for.
//!
//! The page is served by the shell itself under the scheme [`SCHEME`], read
//! from the source tree at each load (edit, then reload with F5 in the
//! inspector). Tauri counts a scheme the app registered as a local origin,
//! so the page reaches the window controls through a capability added at
//! runtime ([`grant`]), not the gateway's; a file in capabilities/ would be
//! built into release builds too. The main window's navigation guard admits
//! the scheme only while the demo is on. A release build ignores the
//! variable and compiles neither the scheme handler nor the source path.

#[cfg(debug_assertions)]
use tauri::http::{header, Response, StatusCode};

/// The variable that asks for the demo.
pub(crate) const DEMO_VAR: &str = "LMGW_WINDOW_DEMO";

/// The scheme the demo page is served under.
pub(crate) const SCHEME: &str = "lmgw-demo";

/// The sample, in the source tree this debug build was made from.
#[cfg(debug_assertions)]
const SAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/design/ui-rebuild/modern-sample.html"
);

/// `1` asks for the demo; unset or empty does not; anything else is an
/// error naming the value.
pub(crate) fn parse(raw: Option<&str>) -> Result<bool, String> {
    match raw.map(str::trim) {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(other.to_string()),
    }
}

/// Whether the main window shows the demo, read once.
pub(crate) fn wanted() -> bool {
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WANTED.get_or_init(|| match parse(std::env::var(DEMO_VAR).ok().as_deref()) {
        Ok(false) => false,
        Ok(true) if !cfg!(debug_assertions) => {
            tracing::warn!("{DEMO_VAR}=1: the design sample is a debug build's; ignored");
            false
        }
        Ok(true) => {
            #[cfg(debug_assertions)]
            tracing::info!("{DEMO_VAR}=1: the main window shows {SAMPLE}");
            true
        }
        Err(value) => {
            tracing::warn!("{DEMO_VAR}={value:?} is neither 1 nor unset; the dashboard loads");
            false
        }
    })
}

/// Where the main window opens in demo mode.
pub(crate) fn url() -> String {
    format!("{SCHEME}://localhost/")
}

/// Whether `url` is the demo's while the demo is on.
pub(crate) fn admits(url: &tauri::Url) -> bool {
    wanted() && url.scheme() == SCHEME
}

/// Whether the window's document is the demo's: the demo is on and both the
/// committed and the active URI are of the demo's scheme, as the gateway's
/// own check asks of the gateway (`media::document_at_gateway`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn document_is_demo(
    wanted: bool,
    committed: Option<&str>,
    active: Option<&str>,
) -> bool {
    let at = |u: Option<&str>| {
        u.and_then(|u| u.parse::<tauri::Url>().ok())
            .is_some_and(|u| u.scheme() == SCHEME)
    };
    wanted && at(committed) && at(active)
}

/// The capability that lets the demo page drive the window controls and
/// report that it has drawn itself (the shell shows the window then). No
/// other local page exists: the app bundles no frontend.
#[cfg(debug_assertions)]
const CAPABILITY: &str = r#"{
  "identifier": "window-demo",
  "windows": ["main"],
  "permissions": ["core:event:allow-emit", "allow-window-chrome"]
}"#;

/// Adds [`CAPABILITY`] when the demo is on, in a debug build; otherwise does
/// nothing.
pub(crate) fn grant<R: tauri::Runtime>(app: &impl tauri::Manager<R>) -> tauri::Result<()> {
    #[cfg(debug_assertions)]
    if wanted() {
        app.add_capability(CAPABILITY)?;
    }
    #[cfg(not(debug_assertions))]
    let _ = app;
    Ok(())
}

/// Registers the demo's scheme when the demo is on (debug builds).
#[cfg(not(debug_assertions))]
pub(crate) fn register<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
}

/// Registers the demo's scheme when the demo is on (debug builds).
#[cfg(debug_assertions)]
pub(crate) fn register<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    if !wanted() {
        return builder;
    }
    builder.register_uri_scheme_protocol(SCHEME, |_ctx, request| {
        if request.uri().path() != "/" {
            return reply(StatusCode::NOT_FOUND, "text/plain", b"not found".to_vec());
        }
        match std::fs::read(SAMPLE) {
            Ok(body) => reply(StatusCode::OK, "text/html; charset=utf-8", body),
            Err(e) => reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain",
                format!("{SAMPLE}: {e}").into_bytes(),
            ),
        }
    })
}

#[cfg(debug_assertions)]
fn reply(status: StatusCode, kind: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, kind)
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_is_one_and_nothing_else() {
        assert_eq!(parse(None), Ok(false));
        assert_eq!(parse(Some("")), Ok(false));
        assert_eq!(parse(Some("0")), Ok(false));
        assert_eq!(parse(Some("1")), Ok(true));
        assert_eq!(parse(Some(" 1 ")), Ok(true));
        assert_eq!(parse(Some("yes")), Err("yes".to_string()));
    }

    #[test]
    fn only_a_demo_document_is_the_demos() {
        let demo = Some("lmgw-demo://localhost/");
        let gateway = Some("http://127.0.0.1:8871/");
        assert!(document_is_demo(true, demo, demo));
        assert!(!document_is_demo(false, demo, demo), "demo off");
        // A load under way: one URI is the demo's, the other is not.
        assert!(!document_is_demo(true, demo, gateway));
        assert!(!document_is_demo(true, gateway, demo));
        assert!(!document_is_demo(true, gateway, gateway));
        assert!(!document_is_demo(true, None, demo));
        assert!(!document_is_demo(true, demo, None));
        assert!(!document_is_demo(true, Some("lmgw-demo.evil://x/"), demo));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_sample_is_in_the_tree() {
        let html = std::fs::read_to_string(SAMPLE).expect("the design sample");
        assert!(
            html.contains("data-window-header"),
            "the sample marks its headers"
        );
    }
}
